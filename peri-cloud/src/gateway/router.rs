use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use peri_acp_types::messages::MessageContent;
use peri_acp_types::permission::PermissionMode;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use uuid::Uuid;

use super::broker::{InteractionHub, SessionBroker};
use super::store::GatewayStore;
use super::{
    ChannelRoute, DeliveryBody, DeliveryError, GatewayError, GatewayReceipt, GatewayResult,
    InboundMessage, InteractionAction, MessageAdapter, SessionConnector,
};
use crate::identity::{DeviceRecord, IdentityError, IdentityService};
use crate::state::{StateError, TurnState};
use crate::{CloudRuntime, SubmitTurn};

type Origins = HashMap<Uuid, Arc<RwLock<Option<String>>>>;
#[path = "menus.rs"]
mod menus;
#[path = "workspaces.rs"]
mod workspaces;
pub use workspaces::WorkspaceSelection;
struct Ownership {
    closing: bool,
    unconfirmed: bool,
    tasks: Vec<JoinHandle<GatewayResult<()>>>,
}

/// One deployment-owned router. Incoming HTTP/QQ waiters have no authority to
/// abort its admissions, selection changes or delivery. Drain this owner before
/// CloudRuntime shutdown closes the shared journal.
pub struct Gateway {
    runtime: Arc<CloudRuntime>,
    identity: Arc<IdentityService>,
    connector: Arc<dyn SessionConnector>,
    adapters: HashMap<String, Arc<dyn MessageAdapter>>,
    store: GatewayStore,
    hub: Arc<InteractionHub>,
    origins: RwLock<Origins>,
    owner: Mutex<Ownership>,
    receive_gate: tokio::sync::Mutex<()>,
    dispatch_gate: tokio::sync::Mutex<()>,
    max_iterations: usize,
}

impl Gateway {
    pub async fn new(
        runtime: Arc<CloudRuntime>,
        identity: Arc<IdentityService>,
        connector: Arc<dyn SessionConnector>,
        adapters: Vec<Arc<dyn MessageAdapter>>,
        max_iterations: usize,
    ) -> GatewayResult<Arc<Self>> {
        if max_iterations == 0 {
            return Err(GatewayError::Invalid);
        }
        let mut mapped = HashMap::new();
        for adapter in adapters {
            let id = adapter.instance_id();
            if id.is_empty() || id.len() > 128 || id.contains('\0') || mapped.contains_key(id) {
                return Err(GatewayError::Invalid);
            }
            mapped.insert(id.to_owned(), adapter);
        }
        if !runtime.journal().claim_gateway() {
            return Err(GatewayError::Busy);
        }
        let store = GatewayStore::new(runtime.journal());
        store.recover().await?;
        let hub = InteractionHub::new(
            identity.clone(),
            runtime.journal().clone(),
            Arc::downgrade(&runtime),
            store.clone(),
        );
        Ok(Arc::new(Self {
            runtime,
            identity,
            connector,
            adapters: mapped,
            store,
            hub,
            origins: RwLock::new(HashMap::new()),
            owner: Mutex::new(Ownership {
                closing: false,
                unconfirmed: false,
                tasks: Vec::new(),
            }),
            receive_gate: tokio::sync::Mutex::new(()),
            dispatch_gate: tokio::sync::Mutex::new(()),
            max_iterations,
        }))
    }

    pub async fn receive(
        self: &Arc<Self>,
        message: InboundMessage,
    ) -> GatewayResult<GatewayReceipt> {
        message.key()?;
        if !self
            .adapters
            .contains_key(&message.route.identity.adapter_instance_id)
        {
            return Err(GatewayError::Invalid);
        }
        self.reap().await?;
        let (reply, receipt) = oneshot::channel();
        {
            let mut owner = self.owner.lock();
            if owner.closing {
                return Err(GatewayError::Closing);
            }
            if owner.tasks.len() >= 128 {
                return Err(GatewayError::Busy);
            }
            let gateway = self.clone();
            owner.tasks.push(tokio::spawn(async move {
                let result = gateway.process(message).await;
                let unconfirmed = result.as_ref().is_err_and(|error| {
                    matches!(
                        error,
                        GatewayError::Database(_)
                            | GatewayError::Encoding(_)
                            | GatewayError::RecoveryRequired
                    )
                });
                let _ = reply.send(result);
                if unconfirmed {
                    Err(GatewayError::RecoveryRequired)
                } else {
                    Ok(())
                }
            }));
        }
        receipt.await.map_err(|_| GatewayError::RecoveryRequired)?
    }

    async fn process(&self, mut message: InboundMessage) -> GatewayResult<GatewayReceipt> {
        // Personal cloud deployments have few channels. This gate serializes
        // routing/control operations; long-running model/tool work is elsewhere.
        let _gate = self.receive_gate.lock().await;
        let (receipt, created) = self.store.admit(&message).await?;
        if !created {
            if let Some(session) = receipt.session_id {
                let principal = self
                    .identity
                    .resolve_channel(&message.route.identity)
                    .await?;
                self.runtime.journal().session(principal, session).await?;
            }
            return Ok(receipt);
        }
        let routed = async {
            if let Some(index) = message.text.trim().strip_prefix("/选 ") {
                let principal = self
                    .identity
                    .resolve_channel(&message.route.identity)
                    .await?;
                let token = self
                    .store
                    .menu_number(
                        &message.route,
                        principal,
                        index.parse().map_err(|_| GatewayError::Invalid)?,
                    )
                    .await?;
                message.text = format!("/卡片 {token}");
            }
            self.route(&message).await
        }
        .await;
        match routed {
            Ok(Some((text, principal))) => self.store.notice(&message, text, principal).await?,
            Ok(None) => (),
            Err(error) => {
                // Admission can commit even if its receipt/update fails. Link
                // that exact existing turn instead of hiding it behind a notice.
                if self.store.link_existing(&receipt.event_key).await? {
                    return self.store.receipt(&receipt.event_key).await;
                }
                let text = match error {
                    GatewayError::Identity(IdentityError::Unauthorized | IdentityError::Forbidden) => "请先在电脑登录并生成关联口令，然后发送 /绑定 口令；也请检查账号或实例是否已撤销。",
                    GatewayError::Identity(IdentityError::RateLimited) => "请求过于频繁，请稍后重试。",
                    GatewayError::Busy | GatewayError::State(StateError::Busy) => "当前任务仍在运行或执行状态未确认；请先等待完成或发送 /取消。",
                    GatewayError::Connection => "无法建立工作区连接；请检查执行器、SSH 连接，以及路径是否是实例上存在且可访问的目录。",
                    GatewayError::Invalid => "参数不正确。发送“菜单”打开操作卡片。",
                    GatewayError::StaleInteraction => "卡片已过期、已使用或会话已改变。请发送“菜单”重新打开。",
                    _ => "这次请求未能完成，请检查云服务状态。没有自动重试执行。",
                };
                self.store.notice(&message, text.into(), None).await?;
            }
        }
        self.store.receipt(&receipt.event_key).await
    }

    async fn route(
        &self,
        message: &InboundMessage,
    ) -> GatewayResult<Option<(DeliveryBody, Option<Uuid>)>> {
        let text = message.text.trim();
        if let Some(code) = text.strip_prefix("/绑定 ") {
            self.identity
                .claim_pair_code(message.route.identity.clone(), code.trim())
                .await?;
            return Ok(Some((
                "关联请求已发到登录页面，请确认这次 QQ 身份。确认后发送“菜单”选择实例。".into(),
                None,
            )));
        }
        if matches!(text, "/帮助" | "菜单" | "/菜单")
            && self
                .identity
                .resolve_channel(&message.route.identity)
                .await
                .is_err()
        {
            return Ok(Some(("先在个人电脑的登录入口生成关联口令，再发送 /绑定 口令，回到网页确认账号。关联后发送“菜单”，即可选择实例、工作区和权限；选好后直接聊天。".into(), None)));
        }
        let (principal, devices) = self
            .identity
            .channel_devices(&message.route.identity)
            .await?;
        let selected = self.store.selection(&message.route, principal).await?;
        let action = match text {
            "/菜单" | "菜单" | "/帮助" => Some(super::menu_store::MenuAction::Home),
            "/实例" | "实例" | "/电脑" => {
                Some(super::menu_store::MenuAction::Instances { offset: 0 })
            }
            "/工作区" | "工作区" => Some(super::menu_store::MenuAction::Workspaces),
            "/权限" => Some(super::menu_store::MenuAction::Permissions),
            _ => None,
        };
        if let Some(action) = action {
            return Ok(Some((
                self.menu_route(message, principal, &devices, selected, action)
                    .await?,
                Some(principal),
            )));
        }
        if let Some(token) = text.strip_prefix("/卡片 ") {
            let token = Uuid::parse_str(token).map_err(|_| GatewayError::Invalid)?;
            let record = self
                .store
                .menu_action(&message.route, principal, token, self.identity.now())
                .await?;
            if record.action.mutates() && record.expected_session != selected {
                return Err(GatewayError::StaleInteraction);
            }
            self.store
                .consume_menu_action(token, &message.event_id)
                .await?;
            return Ok(Some((
                self.menu_route(message, principal, &devices, selected, record.action)
                    .await?,
                Some(principal),
            )));
        }
        if let Some(input) = text.strip_prefix("/连接 ") {
            let (id, workspace) = workspaces::connection_arguments(input)?;
            let device = devices
                .iter()
                .find(|d| d.id == id)
                .ok_or(IdentityError::Forbidden)?;
            let frozen = self
                .select_instance(&message.route, principal, device, selected, workspace)
                .await?;
            return Ok(Some((
                format!(
                    "已连接实例：{}\n工作区：{}\n可直接发送任务，或发送“菜单”选择目录与权限。",
                    device.name, frozen.binding.workspace
                )
                .into(),
                Some(principal),
            )));
        }
        let Some(session) = selected else {
            return Ok(Some((
                self.menu_route(
                    message,
                    principal,
                    &devices,
                    None,
                    super::menu_store::MenuAction::Instances { offset: 0 },
                )
                .await?,
                Some(principal),
            )));
        };
        let state = self.runtime.journal().session(principal, session).await?;
        let device = devices
            .iter()
            .find(|device| device.id == state.frozen.device_id)
            .ok_or(IdentityError::Forbidden)?;
        if let Some(path) = text.strip_prefix("/工作区 ") {
            let frozen = self
                .change_workspace(&message.route, principal, device, session, path.trim())
                .await?;
            return Ok(Some((
                format!(
                    "已切换工作区：{}\n已开始新会话，保留当前权限设置。旧会话历史已保存。",
                    frozen.binding.workspace
                )
                .into(),
                Some(principal),
            )));
        }
        if text == "/状态" {
            let active = self
                .runtime
                .journal()
                .active_turn(principal, session)
                .await?;
            let status = match active.map(|turn| turn.state) {
                Some(TurnState::RecoveryRequired) => "执行状态待恢复确认",
                Some(_) => "任务运行中",
                None if !state.execution_settled() => "实例任务尚未确认全部结束",
                None => "空闲",
            };
            return Ok(Some((
                format!(
                    "实例：{}\n工作区：{}\n权限：{}\n状态：{status}",
                    device.name,
                    state.frozen.binding.workspace,
                    permission_label(state.permissions()?)
                )
                .into(),
                Some(principal),
            )));
        }
        if text == "/取消" {
            if let Some(turn) = self
                .runtime
                .journal()
                .active_turn(principal, session)
                .await?
            {
                self.runtime
                    .cancel(principal, session, turn.turn_id)
                    .await?;
                return Ok(Some((
                    "已提交取消请求；是否停止将以执行器返回的任务状态为准。".into(),
                    Some(principal),
                )));
            }
            return Ok(Some((
                "没有正在运行的云端任务。实例后台执行状态仍以执行器为准。".into(),
                Some(principal),
            )));
        }
        if let Some(mode) = text.strip_prefix("/权限 ") {
            let mode = match mode.trim() {
                "默认" => PermissionMode::Default,
                "编辑" => PermissionMode::AcceptEdit,
                "全部" => PermissionMode::Bypass,
                _ => return Err(GatewayError::Invalid),
            };
            self.runtime
                .journal()
                .set_permissions(principal, session, mode)
                .await?;
            return Ok(Some((
                format!("当前会话权限已设为{}。", permission_label(mode)).into(),
                Some(principal),
            )));
        }
        if text.starts_with('/') {
            return Err(GatewayError::Invalid);
        }
        if !state.execution_settled()
            || self.runtime.journal().has_unconfirmed_turn(session).await?
        {
            return Err(GatewayError::Busy);
        }
        if !self.origins.read().contains_key(&session) {
            let agent = self
                .connector
                .open(principal, device, Some(&state.frozen))
                .await?;
            self.attach(&message.route, principal, device, agent)
                .await?;
        }
        let origin = self
            .origins
            .read()
            .get(&session)
            .cloned()
            .ok_or(GatewayError::RecoveryRequired)?;
        *origin.write() = Some(message.event_id.clone());
        self.store
            .stage(&message.key()?, principal, session)
            .await?;
        let turn = self
            .runtime
            .submit(SubmitTurn {
                principal_id: principal,
                session_id: session,
                request_key: format!("gateway:{}", message.key()?),
                prompt: MessageContent::Text(message.text.clone()),
                max_iterations: self.max_iterations,
            })
            .await?;
        self.store.submitted(&message.key()?, turn.turn_id).await?;
        Ok(None)
    }

    async fn attach(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
        device: &DeviceRecord,
        agent: Arc<crate::CloudAgent>,
    ) -> GatewayResult<()> {
        let frozen = agent.frozen_session(principal);
        let origin = Arc::new(RwLock::new(None));
        let broker = Arc::new(SessionBroker {
            hub: self.hub.clone(),
            route: route.clone(),
            principal,
            session: frozen.session_id,
            device: device.id,
            device_name: device.name.clone(),
            workspace: frozen.binding.workspace,
            origin: origin.clone(),
        });
        self.runtime
            .attach(principal, agent, PermissionMode::Default, broker, None)
            .await?;
        self.origins.write().insert(frozen.session_id, origin);
        Ok(())
    }

    /// Trusted adapters supply the observed route for platform buttons. A Web
    /// fallback must resolve its authenticated account before calling this path.
    pub async fn respond(
        &self,
        route: &ChannelRoute,
        request: Uuid,
        action: InteractionAction,
    ) -> GatewayResult<()> {
        if self.owner.lock().closing {
            return Err(GatewayError::Closing);
        }
        self.hub.respond(route, request, action).await
    }

    /// Browser identity selects the account; the request never supplies a route.
    pub async fn browser_interactions(
        &self,
        actor: &crate::identity::Authenticated,
    ) -> GatewayResult<Vec<super::InteractionCard>> {
        self.identity.browser_scope(actor).await?;
        self.hub.browser_cards(actor.principal().id).await
    }

    pub async fn respond_browser(
        &self,
        actor: &crate::identity::Authenticated,
        request: Uuid,
        action: InteractionAction,
    ) -> GatewayResult<()> {
        if self.owner.lock().closing {
            return Err(GatewayError::Closing);
        }
        self.identity.browser_scope(actor).await?;
        self.hub
            .respond_browser(actor.principal().id, request, action)
            .await
    }

    /// Safe to call periodically or after an inbound event. Send ownership is
    /// retained if the caller disconnects; unknown delivery is never replayed.
    pub async fn dispatch(self: &Arc<Self>) -> GatewayResult<()> {
        self.reap().await?;
        let (sender, receiver) = oneshot::channel();
        {
            let mut owner = self.owner.lock();
            if owner.closing {
                return Err(GatewayError::Closing);
            }
            if owner.tasks.len() >= 128 {
                return Err(GatewayError::Busy);
            }
            let gateway = self.clone();
            owner.tasks.push(tokio::spawn(async move {
                let result = gateway.dispatch_owned().await;
                let unconfirmed = result.is_err();
                let _ = sender.send(result);
                if unconfirmed {
                    Err(GatewayError::RecoveryRequired)
                } else {
                    Ok(())
                }
            }));
        }
        receiver.await.map_err(|_| GatewayError::RecoveryRequired)?
    }

    async fn dispatch_owned(&self) -> GatewayResult<()> {
        let _gate = self.dispatch_gate.lock().await;
        for pending in self.store.pending_turns().await? {
            let turn = self
                .runtime
                .journal()
                .turn(pending.principal, pending.session, pending.turn)
                .await?;
            let session = self
                .runtime
                .journal()
                .session(pending.principal, pending.session)
                .await?;
            self.store
                .project(&pending, &turn, session.frozen.device_id)
                .await?;
        }
        for pending in self.store.ready().await? {
            let Some(adapter) = self
                .adapters
                .get(&pending.delivery.route.identity.adapter_instance_id)
            else {
                continue;
            };
            let authorized = if let Some(principal) = pending.principal {
                let resolved = match pending.device {
                    Some(device) => self
                        .identity
                        .channel_device(&pending.delivery.route.identity, device)
                        .await
                        .map(|(id, _)| id),
                    None => {
                        self.identity
                            .resolve_channel(&pending.delivery.route.identity)
                            .await
                    }
                };
                match resolved {
                    Ok(id) => id == principal,
                    Err(IdentityError::Unauthorized | IdentityError::Forbidden) => false,
                    Err(error) => return Err(error.into()),
                }
            } else {
                true
            };
            let live = match &pending.delivery.body {
                DeliveryBody::Interaction { card } => self.hub.is_live(card.request_id),
                _ => true,
            };
            if !authorized || !live {
                if let DeliveryBody::Interaction { card } = &pending.delivery.body {
                    self.hub.invalidate(card.request_id);
                }
                self.store.delivered(&pending.key, "suppressed").await?;
                continue;
            }
            if !self.store.begin_send(&pending.key).await? {
                continue;
            }
            let state = match adapter.deliver(&pending.delivery).await {
                Ok(()) => "delivered",
                Err(DeliveryError::Rejected) => {
                    if let DeliveryBody::Interaction { card } = &pending.delivery.body {
                        self.hub.invalidate(card.request_id);
                    }
                    "rejected"
                }
                Err(DeliveryError::Unconfirmed) => "unconfirmed",
            };
            self.store.delivered(&pending.key, state).await?;
        }
        Ok(())
    }

    async fn reap(&self) -> GatewayResult<()> {
        let handles = {
            let mut owner = self.owner.lock();
            let mut ready = Vec::new();
            let mut index = 0;
            while index < owner.tasks.len() {
                if owner.tasks[index].is_finished() {
                    ready.push(owner.tasks.swap_remove(index));
                } else {
                    index += 1;
                }
            }
            ready
        };
        for handle in handles {
            if !matches!(handle.await, Ok(Ok(()))) {
                self.owner.lock().unconfirmed = true;
            }
        }
        if self.owner.lock().unconfirmed {
            Err(GatewayError::RecoveryRequired)
        } else {
            Ok(())
        }
    }

    pub async fn shutdown(&self, budget: Duration) -> GatewayResult<bool> {
        self.owner.lock().closing = true;
        let deadline = Instant::now() + budget;
        loop {
            self.reap().await?;
            if self.owner.lock().tasks.is_empty() {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

fn permission_label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "默认审批",
        PermissionMode::AcceptEdit => "允许文件编辑",
        PermissionMode::Bypass => "允许全部工具",
        PermissionMode::AutoMode => "模型自动判断",
    }
}
