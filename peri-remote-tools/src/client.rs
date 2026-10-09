use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use peri_acp_types::device_executor::*;
use reqwest::{Client, Method, StatusCode};
use serde::de::DeserializeOwned;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("executor endpoint must listen on a loopback address")]
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
    http: Client,
    endpoint: RwLock<SocketAddr>,
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
        check_endpoint(endpoint)?;
        let http = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Error::Protocol)?;
        let device: ExecutorInfo =
            send(&http, endpoint, &token, Method::GET, "/v1/info", None).await?;
        validate_device(&device, expected_device)?;
        Ok(Arc::new(Self {
            http,
            endpoint: RwLock::new(endpoint),
            reconnect_gate: Mutex::new(()),
            token,
            device,
        }))
    }

    pub fn info(&self) -> &ExecutorInfo {
        &self.device
    }

    /// A new SSH forwarding port is published only after checking device identity.
    pub async fn reconnect(&self, endpoint: SocketAddr) -> Result<()> {
        let _gate = self.reconnect_gate.lock().await;
        check_endpoint(endpoint)?;
        let info: ExecutorInfo = send(
            &self.http,
            endpoint,
            &self.token,
            Method::GET,
            "/v1/info",
            None,
        )
        .await?;
        validate_device(&info, self.device.device_id)?;
        *self.endpoint.write().await = endpoint;
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
        let endpoint = *self.endpoint.read().await;
        send(&self.http, endpoint, &self.token, method, path, body).await
    }
}

fn check_endpoint(endpoint: SocketAddr) -> Result<()> {
    if !endpoint.ip().is_loopback() || endpoint.port() == 0 {
        return Err(Error::NonLoopback);
    }
    Ok(())
}

fn validate_device(info: &ExecutorInfo, expected: Uuid) -> Result<()> {
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

async fn send<T: DeserializeOwned>(
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
