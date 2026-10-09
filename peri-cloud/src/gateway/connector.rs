use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use peri_model::Model;
use peri_remote_tools::{DeviceClient, RemoteSession};
use uuid::Uuid;

use super::{GatewayError, GatewayResult};
use crate::identity::DeviceRecord;
use crate::state::FrozenSession;
use crate::CloudAgent;

/// Connection manager's deployment boundary. A channel cannot supply endpoint,
/// SSH login or bearer tokens. Existing frozen sessions must reopen unchanged.
#[async_trait]
pub trait SessionConnector: Send + Sync {
    async fn open(
        &self,
        principal: Uuid,
        device: &DeviceRecord,
        previous: Option<&FrozenSession>,
    ) -> GatewayResult<Arc<CloudAgent>>;

    /// An account-owned path is a session choice, never a transport setting.
    /// Reopening an existing session still uses `open` with its frozen binding.
    async fn open_workspace(
        &self,
        principal: Uuid,
        device: &DeviceRecord,
        workspace: &str,
    ) -> GatewayResult<Arc<CloudAgent>> {
        let mut selected = device.clone();
        selected.default_workspace = workspace.to_owned();
        self.open(principal, &selected, None).await
    }
}

/// Real native tools on an identity-verified loopback SSH forward. Deployment
/// registers clients; automatic SSH establishment is a separate host concern.
pub struct NativeSessionConnector {
    clients: RwLock<HashMap<Uuid, Arc<DeviceClient>>>,
    model: Arc<dyn Model>,
    system_prompt: String,
    context_window: u32,
}

impl NativeSessionConnector {
    pub fn new(
        model: Arc<dyn Model>,
        system_prompt: String,
        context_window: u32,
    ) -> GatewayResult<Arc<Self>> {
        if context_window == 0 {
            return Err(GatewayError::Invalid);
        }
        Ok(Arc::new(Self {
            clients: RwLock::new(HashMap::new()),
            model,
            system_prompt,
            context_window,
        }))
    }

    pub fn register_client(&self, client: Arc<DeviceClient>) {
        self.clients.write().insert(client.info().device_id, client);
    }
}

#[async_trait]
impl SessionConnector for NativeSessionConnector {
    async fn open(
        &self,
        principal: Uuid,
        device: &DeviceRecord,
        previous: Option<&FrozenSession>,
    ) -> GatewayResult<Arc<CloudAgent>> {
        if device.revoked {
            return Err(GatewayError::Connection);
        }
        let client = self
            .clients
            .read()
            .get(&device.id)
            .cloned()
            .ok_or(GatewayError::Connection)?;
        if client.info().platform != device.platform {
            return Err(GatewayError::Connection);
        }
        let (session_id, executor_id, workspace, prompt, window) = match previous {
            Some(frozen) if frozen.principal_id == principal && frozen.device_id == device.id => (
                frozen.session_id,
                frozen.binding.session_id,
                frozen.binding.workspace.clone(),
                frozen.system_prompt.clone(),
                frozen.context_window,
            ),
            Some(_) => return Err(GatewayError::Connection),
            None => (
                Uuid::new_v4(),
                Uuid::new_v4(),
                device.default_workspace.clone(),
                self.system_prompt.clone(),
                self.context_window,
            ),
        };
        let remote = RemoteSession::open(client, executor_id, session_id.to_string(), &workspace)
            .await
            .map_err(|_| GatewayError::Connection)?;
        let agent = CloudAgent::new(session_id, remote, self.model.clone(), prompt, window)
            .map_err(|_| GatewayError::Connection)?;
        if previous.is_some_and(|frozen| *frozen != agent.frozen_session(principal)) {
            return Err(GatewayError::Connection);
        }
        Ok(Arc::new(agent))
    }
}
