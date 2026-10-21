use std::net::SocketAddr;
use std::sync::Arc;

use crate::transport::{self, Connection, ExecutorEndpoint};
use peri_acp_types::device_executor::*;
use reqwest::{Client, Method, StatusCode};
use serde::de::DeserializeOwned;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("executor endpoint requires loopback or an explicit private WireGuard route")]
    NonLoopback,
    #[error("executor connection unavailable; query the existing invocation before retrying")]
    Disconnected,
    #[error("executor rejected the request (HTTP {0})")]
    Rejected(u16),
    #[error("executor device identity or protocol does not match the bound device")]
    DeviceMismatch,
    #[error("invalid executor protocol response")]
    Protocol,
    #[error("executor admission remains unconfirmed; query the existing invocation")]
    UnconfirmedExecution,
    #[error("canonical session, turn and invocation identity are required")]
    MissingIdentity,
    #[error("tool invocation belongs to a different cloud session")]
    SessionMismatch,
}

pub type Result<T> = std::result::Result<T, Error>;

/// Connection credentials are private and deliberately have no Debug projection.
pub struct DeviceClient {
    connection: RwLock<Connection>,
    routes: RwLock<Vec<ExecutorEndpoint>>,
    reconnect_gate: Mutex<()>,
    token: String,
    device: ExecutorInfo,
}

impl DeviceClient {
    pub async fn connect(
        endpoint: SocketAddr,
        token: String,
        expected_device: Uuid,
    ) -> Result<Arc<Self>> {
        Self::connect_routes(
            vec![ExecutorEndpoint::Loopback(endpoint)],
            token,
            expected_device,
        )
        .await
    }

    pub async fn connect_routes(
        routes: Vec<ExecutorEndpoint>,
        token: String,
        expected_device: Uuid,
    ) -> Result<Arc<Self>> {
        let (connection, device) = transport::select(&routes, &token, expected_device).await?;
        Ok(Arc::new(Self {
            connection: RwLock::new(connection),
            routes: RwLock::new(routes),
            reconnect_gate: Mutex::new(()),
            token,
            device,
        }))
    }

    pub fn info(&self) -> &ExecutorInfo {
        &self.device
    }

    /// Operator diagnostics only; never included in a model tool schema.
    pub async fn active_transport(&self) -> &'static str {
        self.connection.read().await.kind()
    }

    pub async fn directories(&self, query: DirectoryQuery) -> Result<DirectoryListing> {
        let body = serde_json::to_value(query).map_err(|_| Error::Protocol)?;
        self.request(Method::POST, "/v1/directories", Some(body))
            .await
    }

    /// A new SSH forwarding port is published only after checking device identity.
    pub async fn reconnect(&self, endpoint: SocketAddr) -> Result<()> {
        let _gate = self.reconnect_gate.lock().await;
        let routes = vec![ExecutorEndpoint::Loopback(endpoint)];
        let (connection, _) =
            transport::select(&routes, &self.token, self.device.device_id).await?;
        *self.connection.write().await = connection;
        *self.routes.write().await = routes;
        Ok(())
    }

    /// Refresh transport without replacing frozen sessions or uncertain task ownership.
    pub async fn refresh_connection(&self) -> Result<()> {
        let _gate = self.reconnect_gate.lock().await;
        let routes = self.routes.read().await.clone();
        let (connection, _) =
            transport::select(&routes, &self.token, self.device.device_id).await?;
        *self.connection.write().await = connection;
        Ok(())
    }

    pub async fn bind(&self, id: Uuid, workspace: &str) -> Result<SessionBinding> {
        let body = serde_json::json!({"workspace": workspace});
        let binding: SessionBinding = self
            .request(Method::PUT, &format!("/v1/sessions/{id}"), Some(body))
            .await?;
        if binding.session_id != id || binding.workspace.is_empty() {
            return Err(Error::Protocol);
        }
        Ok(binding)
    }

    pub async fn catalog(&self, id: Uuid) -> Result<Vec<NativeToolDescriptor>> {
        self.request(Method::GET, &format!("/v1/sessions/{id}/tools"), None)
            .await
    }

    /// Older executors do not serve the workspace-read endpoints; callers must
    /// treat the capability as absent rather than probing and retrying.
    pub fn supports_workspace_read(&self) -> bool {
        self.device
            .features
            .iter()
            .any(|feature| feature == WORKSPACE_READ_FEATURE)
    }

    pub async fn environment(&self, binding: &SessionBinding) -> Result<EnvironmentSnapshot> {
        if !self
            .device
            .features
            .iter()
            .any(|f| f == ENVIRONMENT_READ_FEATURE)
        {
            return Err(Error::Protocol);
        }
        let value: EnvironmentSnapshot = self
            .request(
                Method::GET,
                &format!("/v1/sessions/{}/environment", binding.session_id),
                None,
            )
            .await?;
        if value.device_id != self.device.device_id
            || value.binding != *binding
            || value.platform != self.device.platform
            || value.applications.len() > 32
            || serde_json::to_vec(&value)
                .map_err(|_| Error::Protocol)?
                .len()
                > 16384
        {
            return Err(Error::Protocol);
        }
        Ok(value)
    }

    pub async fn workspace_instructions(&self, id: Uuid) -> Result<WorkspaceInstructions> {
        self.request(
            Method::GET,
            &format!("/v1/sessions/{id}/workspace/instructions"),
            None,
        )
        .await
    }

    pub async fn workspace_skills(&self, id: Uuid) -> Result<WorkspaceSkills> {
        self.request(
            Method::GET,
            &format!("/v1/sessions/{id}/workspace/skills"),
            None,
        )
        .await
    }

    pub async fn workspace_read(&self, id: Uuid, path: &str) -> Result<WorkspaceFile> {
        let body = serde_json::json!({ "path": path });
        self.request(
            Method::POST,
            &format!("/v1/sessions/{id}/workspace/read"),
            Some(body),
        )
        .await
    }

    pub async fn submit(&self, request: &SubmitJob) -> Result<JobReceipt> {
        let body = serde_json::to_value(request).map_err(|_| Error::Protocol)?;
        let receipt: JobReceipt = self.request(Method::POST, "/v1/jobs", Some(body)).await?;
        check_job(&receipt.job, request.session_id, request.invocation_id)?;
        Ok(receipt)
    }

    pub async fn job(&self, session: Uuid, id: Uuid) -> Result<JobSnapshot> {
        let job = self
            .request(Method::GET, &format!("/v1/jobs/{id}"), None)
            .await?;
        check_job(&job, session, id)?;
        Ok(job)
    }

    pub async fn cancel(&self, session: Uuid, id: Uuid) -> Result<JobSnapshot> {
        // Check ownership before sending a state-changing request.
        self.job(session, id).await?;
        let job = self
            .request(Method::POST, &format!("/v1/jobs/{id}/cancel"), None)
            .await?;
        check_job(&job, session, id)?;
        Ok(job)
    }

    pub async fn jobs(&self, session: Uuid) -> Result<Vec<JobSnapshot>> {
        let jobs: Vec<JobSnapshot> = self
            .request(Method::GET, &format!("/v1/sessions/{session}/jobs"), None)
            .await?;
        if jobs.iter().any(|job| job.session_id != session) {
            return Err(Error::Protocol);
        }
        Ok(jobs)
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<T> {
        let connection = self.connection.read().await.clone();
        let result = async {
            connection.verify_route().await?;
            send(
                &connection.http,
                connection.endpoint,
                &self.token,
                method.clone(),
                path,
                body.clone(),
            )
            .await
        }
        .await;
        if matches!(result, Err(Error::Disconnected)) {
            // Only reads may be retried. A lost write may already have executed.
            if method == Method::GET {
                self.refresh_connection().await?;
                let connection = self.connection.read().await.clone();
                connection.verify_route().await?;
                return send(
                    &connection.http,
                    connection.endpoint,
                    &self.token,
                    method,
                    path,
                    body,
                )
                .await;
            }
            // Preserve ambiguity even if the replacement route is unavailable or rejected.
            let _ = self.refresh_connection().await;
        }
        result
    }
}

pub(crate) fn validate_device(info: &ExecutorInfo, expected: Uuid) -> Result<()> {
    if info.device_id != expected
        || info.protocol_version != PROTOCOL_VERSION
        || !matches!(info.platform.as_str(), "windows" | "linux")
    {
        return Err(Error::DeviceMismatch);
    }
    Ok(())
}

fn check_job(job: &JobSnapshot, session: Uuid, id: Uuid) -> Result<()> {
    if job.session_id != session || job.invocation_id != id {
        return Err(Error::Protocol);
    }
    Ok(())
}

pub(crate) async fn send<T: DeserializeOwned>(
    http: &Client,
    endpoint: SocketAddr,
    token: &str,
    method: Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<T> {
    let mut request = http
        .request(method, format!("http://{endpoint}{path}"))
        .bearer_auth(token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.map_err(|_| Error::Disconnected)?;
    if response.status() == StatusCode::NOT_FOUND {
        return Err(Error::Rejected(404));
    }
    if !response.status().is_success() {
        return Err(Error::Rejected(response.status().as_u16()));
    }
    response.json().await.map_err(|_| Error::Protocol)
}
