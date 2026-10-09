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
use crate::gateway::{GatewayError, GatewayResult, NativeSessionConnector, SessionConnector};
use crate::identity::DeviceRecord;
use crate::state::FrozenSession;
use crate::CloudAgent;

pub(crate) struct Connections {
    native: Arc<NativeSessionConnector>,
    configured: HashMap<Uuid, DeviceConnection>,
    clients: tokio::sync::Mutex<HashMap<Uuid, Arc<DeviceClient>>>,
}

impl Connections {
    pub fn new(
        devices: Vec<DeviceConnection>,
        model: Arc<dyn Model>,
        prompt: String,
        window: u32,
    ) -> HostResult<Arc<Self>> {
        let native = NativeSessionConnector::new(model, prompt, window)
            .map_err(|_| HostError::Configuration)?;
        Ok(Arc::new(Self {
            native,
            configured: devices
                .into_iter()
                .map(|device| (device.device_id, device))
                .collect(),
            clients: tokio::sync::Mutex::new(HashMap::new()),
        }))
    }

    /// Offline devices do not prevent account/QQ startup. Existing clients are
    /// identity-verified again when selecting/restoring a frozen session.
    async fn client(&self, id: Uuid) -> GatewayResult<Arc<DeviceClient>> {
        let config = self.configured.get(&id).ok_or(GatewayError::Connection)?;
        let mut clients = self.clients.lock().await;
        if let Some(client) = clients.get(&id) {
            client
                .reconnect(config.endpoint)
                .await
                .map_err(|_| GatewayError::Connection)?;
            return Ok(client.clone());
        }
        let metadata = tokio::fs::metadata(&config.token_file)
            .await
            .map_err(|_| GatewayError::Connection)?;
        if !metadata.is_file() || metadata.len() > 8192 {
            return Err(GatewayError::Connection);
        }
        let token = tokio::fs::read_to_string(&config.token_file)
            .await
            .map_err(|_| GatewayError::Connection)?;
        let token = token.trim().to_owned();
        if token.is_empty() || token.len() > 8192 {
            return Err(GatewayError::Connection);
        }
        let client = DeviceClient::connect(config.endpoint, token, id)
            .await
            .map_err(|_| GatewayError::Connection)?;
        self.native.register_client(client.clone());
        clients.insert(id, client.clone());
        Ok(client)
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
