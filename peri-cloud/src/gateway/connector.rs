use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use peri_model::Model;
use peri_remote_tools::{DeviceClient, RemoteSession};
use uuid::Uuid;

use super::{ConnectionLease, DeviceInspection, GatewayError, GatewayResult};
use crate::identity::DeviceRecord;
use crate::state::FrozenSession;
use crate::CloudAgent;

/// Deployment-observed transport state. Enrollment is never evidence of a
/// live executor; `NotConfigured` and `Offline` stay distinct.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceReachability {
    #[default]
    Unknown,
    NotConfigured,
    Offline,
    Online,
}

impl DeviceReachability {
    pub fn key(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::NotConfigured => "not_configured",
            Self::Offline => "offline",
            Self::Online => "online",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "",
            Self::NotConfigured => "未接入云端连接",
            Self::Offline => "暂时无法连接",
            Self::Online => "在线",
        }
    }
}

/// Connection manager's deployment boundary. A channel cannot supply endpoint,
/// SSH login or bearer tokens. Existing frozen sessions must reopen unchanged.
#[async_trait]
pub trait SessionConnector: Send + Sync {
    fn context_assembler(&self) -> Option<Arc<crate::context::ContextAssembler>> {
        None
    }
    async fn new_conversation(&self, _principal: Uuid) -> GatewayResult<Arc<CloudAgent>> {
        Err(GatewayError::Connection)
    }
    /// Reuse an already-authorized frozen identity without touching its executor.
    async fn open_conversation(
        &self,
        _principal: Uuid,
        _frozen: &FrozenSession,
    ) -> GatewayResult<Arc<CloudAgent>> {
        Err(GatewayError::Connection)
    }
    fn command_registry(&self) -> Option<Arc<peri_acp_types::command::CommandRegistry>> {
        None
    }
    /// Read-only identity authentication, without opening a session or a job.
    async fn check_connection(&self, _device: &DeviceRecord) -> GatewayResult<&'static str> {
        Err(GatewayError::Connection)
    }
    async fn inspect_device(&self, device: &DeviceRecord) -> GatewayResult<DeviceInspection> {
        Ok(DeviceInspection {
            transport: self.check_connection(device).await?.to_owned(),
            ..Default::default()
        })
    }
    async fn connection_lease(
        &self,
        _principal: Uuid,
        _device: &DeviceRecord,
    ) -> GatewayResult<ConnectionLease> {
        Ok(ConnectionLease::default())
    }
    /// Bounded transport observation; never authorizes a tool or claims work.
    async fn reachability(&self, _device: &DeviceRecord) -> DeviceReachability {
        DeviceReachability::Unknown
    }
    fn account_models(&self) -> Option<Arc<crate::models::AccountModels>> {
        None
    }

    async fn directories(
        &self,
        _principal: Uuid,
        _device: &DeviceRecord,
        _query: peri_acp_types::device_executor::DirectoryQuery,
    ) -> GatewayResult<peri_acp_types::device_executor::DirectoryListing> {
        Err(GatewayError::Connection)
    }
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
    commands: Arc<peri_acp_types::command::CommandRegistry>,
    model: Arc<dyn Model>,
    system_prompt: String,
    context_window: u32,
    account_models: RwLock<Option<Arc<crate::models::AccountModels>>>,
    context_assembler: RwLock<Arc<crate::context::ContextAssembler>>,
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
            commands: crate::commands::native_registry(),
            model,
            system_prompt,
            context_window,
            account_models: RwLock::new(None),
            context_assembler: RwLock::new(Arc::new(crate::context::ContextAssembler::default())),
        }))
    }

    pub fn register_command(
        &self,
        entry: peri_acp_types::command::command_route::RouteEntry,
    ) -> Result<(), peri_acp_types::command::RegisterError> {
        self.commands.register(entry)
    }
    pub fn register_client(&self, client: Arc<DeviceClient>) {
        self.clients.write().insert(client.info().device_id, client);
    }
    pub fn set_account_models(&self, models: Arc<crate::models::AccountModels>) {
        *self.account_models.write() = Some(models);
    }
    pub fn set_context_assembler(&self, assembler: Arc<crate::context::ContextAssembler>) {
        *self.context_assembler.write() = assembler;
    }
}

#[async_trait]
impl SessionConnector for NativeSessionConnector {
    async fn inspect_device(&self, device: &DeviceRecord) -> GatewayResult<DeviceInspection> {
        self.check_connection(device).await?;
        let client = self
            .clients
            .read()
            .get(&device.id)
            .cloned()
            .ok_or(GatewayError::Connection)?;
        inspect_client(&client).await
    }
    async fn connection_lease(
        &self,
        _principal: Uuid,
        device: &DeviceRecord,
    ) -> GatewayResult<ConnectionLease> {
        Ok(ConnectionLease {
            source: if self.clients.read().contains_key(&device.id) {
                "configured"
            } else {
                "none"
            }
            .into(),
            ..Default::default()
        })
    }
    fn context_assembler(&self) -> Option<Arc<crate::context::ContextAssembler>> {
        Some(self.context_assembler.read().clone())
    }
    async fn new_conversation(&self, principal: Uuid) -> GatewayResult<Arc<CloudAgent>> {
        self.open_conversation(
            principal,
            &FrozenSession {
                session_id: Uuid::new_v4(),
                principal_id: principal,
                device_id: None,
                binding: None,
                system_prompt: self.system_prompt.clone(),
                context_window: self.context_window,
            },
        )
        .await
    }
    async fn open_conversation(
        &self,
        principal: Uuid,
        frozen: &FrozenSession,
    ) -> GatewayResult<Arc<CloudAgent>> {
        if principal != frozen.principal_id {
            return Err(crate::identity::IdentityError::Forbidden.into());
        }
        let mut agent = CloudAgent::conversation(frozen, self.model.clone())
            .with_commands(self.commands.clone())
            .with_context_assembler(self.context_assembler.read().clone());
        if let Some(models) = self.account_models.read().clone() {
            agent = agent.with_account_models(models);
        }
        Ok(Arc::new(agent))
    }
    fn command_registry(&self) -> Option<Arc<peri_acp_types::command::CommandRegistry>> {
        Some(self.commands.clone())
    }
    async fn check_connection(&self, device: &DeviceRecord) -> GatewayResult<&'static str> {
        let client = self
            .clients
            .read()
            .get(&device.id)
            .cloned()
            .ok_or(GatewayError::Connection)?;
        if device.revoked || client.info().platform != device.platform {
            return Err(GatewayError::Connection);
        }
        client
            .refresh_connection()
            .await
            .map_err(|_| GatewayError::Connection)?;
        Ok(client.active_transport().await)
    }
    fn account_models(&self) -> Option<Arc<crate::models::AccountModels>> {
        self.account_models.read().clone()
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
        let client = self
            .clients
            .read()
            .get(&device.id)
            .cloned()
            .ok_or(GatewayError::Connection)?;
        if client.info().platform != device.platform {
            return Err(GatewayError::Connection);
        }
        client
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
            Some(frozen)
                if frozen.principal_id == principal && frozen.device_id == Some(device.id) =>
            {
                (
                    frozen.session_id,
                    frozen.device_binding()?.session_id,
                    frozen.workspace(),
                    frozen.system_prompt.clone(),
                    frozen.context_window,
                )
            }
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
        let mut agent = CloudAgent::new(session_id, remote, self.model.clone(), prompt, window)
            .map_err(|_| GatewayError::Connection)?;
        agent = agent
            .with_commands(self.commands.clone())
            .with_context_assembler(self.context_assembler.read().clone());
        if let Some(models) = self.account_models.read().clone() {
            agent = agent.with_account_models(models);
        }
        if previous.is_some_and(|frozen| *frozen != agent.frozen_session(principal)) {
            return Err(GatewayError::Connection);
        }
        Ok(Arc::new(agent))
    }
}

pub(crate) async fn inspect_client(client: &DeviceClient) -> GatewayResult<DeviceInspection> {
    let capabilities = client
        .capabilities()
        .await
        .map_err(|_| GatewayError::Connection)?;
    Ok(DeviceInspection {
        transport: client.active_transport().await.to_owned(),
        protocol_version: capabilities.as_ref().map(|c| c.info.protocol_version),
        version: capabilities.as_ref().map(|c| c.info.version.clone()),
        features: capabilities
            .as_ref()
            .map(|c| c.info.features.clone())
            .unwrap_or_default(),
        tools: capabilities.map(|c| c.tools.into_iter().map(|t| t.definition.name).collect()),
    })
}
