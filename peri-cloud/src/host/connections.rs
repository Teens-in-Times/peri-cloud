use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use peri_model::Model;
use peri_remote_tools::DeviceClient;
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::config::{DeviceConnection, SshConnection};
use super::{HostError, HostResult};
use crate::gateway::{
    DeviceReachability, GatewayError, GatewayResult, NativeSessionConnector, SessionConnector,
};
use crate::identity::{ConnectionStore, DeviceRecord};
use crate::state::FrozenSession;
use crate::CloudAgent;

pub(crate) struct Connections {
    native: Arc<NativeSessionConnector>,
    configured: HashMap<Uuid, DeviceConnection>,
    connections: Option<ConnectionStore>,
    clients: HashMap<Uuid, tokio::sync::Mutex<Option<Arc<DeviceClient>>>>,
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0)
}

impl Connections {
    pub fn set_context_assembler(&self, assembler: Arc<crate::context::ContextAssembler>) {
        self.native.set_context_assembler(assembler);
    }
    pub fn set_account_models(&self, models: Arc<crate::models::AccountModels>) {
        self.native.set_account_models(models);
    }
    pub fn new(
        devices: Vec<DeviceConnection>,
        model: Arc<dyn Model>,
        prompt: String,
        window: u32,
        connections: Option<ConnectionStore>,
    ) -> HostResult<Arc<Self>> {
        let native = NativeSessionConnector::new(model, prompt, window)
            .map_err(|_| HostError::Configuration)?;
        Ok(Arc::new(Self {
            native,
            clients: devices
                .iter()
                .map(|device| (device.device_id, tokio::sync::Mutex::new(None)))
                .collect(),
            configured: devices
                .into_iter()
                .map(|device| (device.device_id, device))
                .collect(),
            connections,
        }))
    }

    /// Offline devices do not prevent account/QQ startup. Existing clients are
    /// identity-verified again when selecting/restoring a frozen session.
    async fn client(&self, id: Uuid) -> GatewayResult<Arc<DeviceClient>> {
        // A live device-reported reverse tunnel takes precedence over static
        // deployment configuration; static config remains the fallback for
        // direct SSH or older executors.
        let dynamic = match &self.connections {
            Some(store) => store
                .active(id, unix_now())
                .await
                .map_err(GatewayError::Identity)?,
            None => None,
        };
        let dynamic_selected = dynamic.is_some();
        let (endpoint, token_file, static_routes) = if let Some(connection) = dynamic {
            (connection.endpoint, connection.token_path, None)
        } else if let Some(config) = self.configured.get(&id) {
            (
                config.endpoint,
                config.token_file.clone(),
                Some(config.routes()),
            )
        } else {
            if let Some(store) = &self.connections {
                if store.exists(id).await.map_err(GatewayError::Identity)? {
                    // The device is registered but its lease expired.
                    return Err(GatewayError::Connection);
                }
            }
            return Err(GatewayError::NotConfigured);
        };
        let mut cached = self
            .clients
            .get(&id)
            .ok_or(GatewayError::Connection)?
            .lock()
            .await;
        if let Some(client) = cached.as_ref() {
            if dynamic_selected {
                client
                    .reconnect(endpoint)
                    .await
                    .map_err(|_| GatewayError::Connection)?;
            } else {
                client
                    .refresh_connection()
                    .await
                    .map_err(|_| GatewayError::Connection)?;
            }
            return Ok(client.clone());
        }
        let missing = |dynamic: bool| {
            if dynamic {
                GatewayError::Connection
            } else {
                GatewayError::NotConfigured
            }
        };
        let metadata = tokio::fs::metadata(&token_file)
            .await
            .map_err(|_| missing(dynamic_selected))?;
        if !metadata.is_file() || metadata.len() > 8192 {
            return Err(missing(dynamic_selected));
        }
        let token = tokio::fs::read_to_string(&token_file)
            .await
            .map_err(|_| missing(dynamic_selected))?;
        let token = token.trim().to_owned();
        if token.is_empty() || token.len() > 8192 {
            return Err(missing(dynamic_selected));
        }
        let client = if let Some(routes) = static_routes {
            DeviceClient::connect_routes(routes, token, id).await
        } else {
            DeviceClient::connect(endpoint, token, id).await
        }
        .map_err(|_| GatewayError::Connection)?;
        self.native.register_client(client.clone());
        *cached = Some(client.clone());
        Ok(client)
    }

    /// A bounded identity-verified probe. It reports configuration and transport
    /// reachability only; it never authorizes a tool or claims a task was run.
    async fn probe(&self, id: Uuid) -> DeviceReachability {
        match tokio::time::timeout(Duration::from_millis(700), self.client(id)).await {
            Ok(Ok(_)) => DeviceReachability::Online,
            Ok(Err(GatewayError::NotConfigured)) => DeviceReachability::NotConfigured,
            Ok(Err(_)) | Err(_) => DeviceReachability::Offline,
        }
    }

    /// A deployment owns tunnel handles until runtime device cleanup is complete.
    pub fn tunnels(&self, stop: CancellationToken) -> Vec<JoinHandle<()>> {
        self.configured
            .values()
            .filter_map(|device| {
                device.ssh.as_ref().map(|ssh| {
                    let endpoint = device.endpoint;
                    let ssh = ssh.clone();
                    let stopped = stop.clone();
                    tokio::spawn(async move { tunnel(endpoint, ssh, stopped).await })
                })
            })
            .collect()
    }
}

#[async_trait]
impl SessionConnector for Connections {
    fn context_assembler(&self) -> Option<Arc<crate::context::ContextAssembler>> {
        self.native.context_assembler()
    }
    async fn new_conversation(&self, principal: Uuid) -> GatewayResult<Arc<CloudAgent>> {
        self.native.new_conversation(principal).await
    }
    async fn open_conversation(
        &self,
        principal: Uuid,
        frozen: &FrozenSession,
    ) -> GatewayResult<Arc<CloudAgent>> {
        self.native.open_conversation(principal, frozen).await
    }
    fn command_registry(&self) -> Option<Arc<peri_acp_types::command::CommandRegistry>> {
        self.native.command_registry()
    }
    fn account_models(&self) -> Option<Arc<crate::models::AccountModels>> {
        self.native.account_models()
    }
    async fn check_connection(&self, device: &DeviceRecord) -> GatewayResult<&'static str> {
        if device.revoked {
            return Err(GatewayError::Connection);
        }
        let client = self.client(device.id).await?;
        if client.info().platform != device.platform {
            return Err(GatewayError::Connection);
        }
        Ok(client.active_transport().await)
    }
    async fn reachability(&self, device: &DeviceRecord) -> DeviceReachability {
        if device.revoked {
            return DeviceReachability::Offline;
        }
        self.probe(device.id).await
    }
    async fn directories(
        &self,
        _principal: Uuid,
        device: &DeviceRecord,
        query: peri_acp_types::device_executor::DirectoryQuery,
    ) -> GatewayResult<peri_acp_types::device_executor::DirectoryListing> {
        if device.revoked {
            return Err(GatewayError::Connection);
        }
        self.client(device.id)
            .await?
            .directories(query)
            .await
            .map_err(|_| GatewayError::Connection)
    }
    async fn open(
        &self,
        principal: Uuid,
        device: &DeviceRecord,
        previous: Option<&FrozenSession>,
    ) -> GatewayResult<Arc<CloudAgent>> {
        if device.revoked {
            return Err(GatewayError::Connection);
        }
        self.client(device.id).await?;
        self.native.open(principal, device, previous).await
    }
}

async fn tunnel(endpoint: std::net::SocketAddr, ssh: SshConnection, stop: CancellationToken) {
    let forward = format!("{endpoint}:{}", ssh.remote_executor);
    loop {
        if stop.is_cancelled() {
            return;
        }
        let mut command = Command::new("ssh");
        command
            .args([
                "-N",
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-L",
                &forward,
                &ssh.host_alias,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        match command.spawn() {
            Ok(mut child) => {
                tokio::select! {
                    biased;
                    _ = stop.cancelled() => { let _ = child.kill().await; let _ = child.wait().await; return; }
                    result = child.wait() => { tracing::warn!(component="ssh_tunnel", exited=result.is_ok(), "SSH tunnel ended; reconnecting"); }
                }
            }
            Err(_) => tracing::warn!(
                component = "ssh_tunnel",
                "SSH could not start; reconnecting later"
            ),
        }
        tokio::select! { _ = stop.cancelled() => return, _ = tokio::time::sleep(Duration::from_secs(3)) => {} }
    }
}
