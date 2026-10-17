use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use peri_acp_types::messages::MessageContent;
use peri_acp_types::permission::PermissionMode;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use uuid::Uuid;

use super::broker::{InteractionHub, SessionBroker, SessionOrigin};
use super::store::GatewayStore;
use super::{
    ChannelRoute, DeliveryBody, DeliveryError, GatewayError, GatewayReceipt, GatewayResult,
    InboundMessage, InteractionAction, MessageAdapter, SessionConnector,
};
use crate::identity::{DeviceRecord, IdentityError, IdentityService};
use crate::state::StateError;
use crate::CloudRuntime;

#[path = "command_api.rs"]
mod command_api;
#[path = "history.rs"]
mod history;
pub use command_api::CommandReceipt;
#[path = "commands.rs"]
mod commands;
#[path = "model_controls.rs"]
mod model_controls;
#[path = "task_control.rs"]
mod task_control;
pub use task_control::TaskReceipt;
type Origins = HashMap<Uuid, Arc<RwLock<Option<SessionOrigin>>>>;
#[path = "menus.rs"]
mod menus;
#[path = "workspaces.rs"]
mod workspaces;
pub use workspaces::WorkspaceSelection;
#[path = "activity.rs"]
pub(super) mod activity;
pub use activity::{ConversationActivity, SavedResultPage};
#[path = "controls.rs"]
mod controls;
#[path = "personal.rs"]
mod personal;
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
    pub(super) identity: Arc<IdentityService>,
    pub(super) connector: Arc<dyn SessionConnector>,
    pub(super) adapters: HashMap<String, Arc<dyn MessageAdapter>>,
    pub(super) store: GatewayStore,
    hub: Arc<InteractionHub>,
    origins: RwLock<Origins>,
    owner: Mutex<Ownership>,
    receive_gates: Mutex<HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    dispatch_gate: tokio::sync::Mutex<()>,
    pub(super) access_gate: tokio::sync::RwLock<()>,
    max_iterations: usize,
    control_origin: RwLock<Option<url::Url>>,
}

impl Gateway {
    /// Links carry resource IDs only; the destination repeats browser ownership checks.
    pub fn set_control_origin(&self, origin: &str) -> GatewayResult<()> {
        crate::portal::PortalConfig::new(origin).map_err(|_| GatewayError::Invalid)?;
        *self.control_origin.write() = Some(origin.parse().map_err(|_| GatewayError::Invalid)?);
        Ok(())
    }
    pub(super) fn control_link(&self, session: Option<Uuid>) -> Option<String> {
        let mut url = self.control_origin.read().clone()?;
        if let Some(session) = session {
            url.query_pairs_mut()
                .append_pair("session", &session.to_string());
        }
        url.set_fragment(Some("activity-panel"));
        Some(url.into())
    }
    pub(super) fn account_gate(&self, key: String) -> Arc<tokio::sync::Mutex<()>> {
        let mut gates = self.receive_gates.lock();
        gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = gates.get(&key).and_then(std::sync::Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        gates.insert(key, Arc::downgrade(&gate));
        gate
    }
    pub fn journal(&self) -> &Arc<crate::state::CloudJournal> {
        self.runtime.journal()
    }
    pub fn limits(&self) -> &crate::limits::RuntimeLimits {
        self.runtime.limits()
    }
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
        for (id, adapter) in &mapped {
            store
                .seed_access(id, adapter.initial_access_policy())
                .await?;
        }
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
            receive_gates: Mutex::new(HashMap::new()),
            dispatch_gate: tokio::sync::Mutex::new(()),
            access_gate: tokio::sync::RwLock::new(()),
            max_iterations,
            control_origin: RwLock::new(None),
        }))
    }

    pub async fn receive(
        self: &Arc<Self>,
        message: InboundMessage,
    ) -> GatewayResult<GatewayReceipt> {
        message.key()?;
        if !self.route_allowed(&message.route).await? {
            return Err(IdentityError::Forbidden.into());
        }
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
        // Serialize controls for one account; offline devices must not block
        // unrelated users. Unbound senders retain an independent route scope.
        let key = match self.identity.resolve_channel(&message.route.identity).await {
            Ok(principal) => principal.to_string(),
            Err(IdentityError::Unauthorized | IdentityError::Forbidden) => message.route.key()?,
            Err(error) => return Err(error.into()),
        };
        let gate = self.account_gate(key);
        let _guard = gate.lock().await;
        if !self.route_allowed(&message.route).await? {
            return Err(IdentityError::Forbidden.into());
        }
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
            if let Some(arguments) = message.text.trim().strip_prefix("/选 ") {
                let principal = self
                    .identity
                    .resolve_channel(&message.route.identity)
                    .await?;
                let arguments: Vec<_> = arguments.split_whitespace().collect();
                let token = match arguments.as_slice() {
                    [index] => {
                        self.store
                            .menu_number(
                                &message.route,
                                principal,
                                index.parse().map_err(|_| GatewayError::Invalid)?,
                            )
                            .await?
                    }
                    [snapshot, index] => {
                        self.store
                            .snapshot_number(
                                &message.route,
                                principal,
                                Uuid::parse_str(snapshot).map_err(|_| GatewayError::Invalid)?,
                                index.parse().map_err(|_| GatewayError::Invalid)?,
                            )
                            .await?
                    }
                    _ => return Err(GatewayError::Invalid),
                };
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
                    GatewayError::State(StateError::Limit) | GatewayError::Runtime(crate::RuntimeError::State(StateError::Limit)) => "当前并发或今日额度已达上限，请稍后重试；可在网页查看自己的用量。",
                    GatewayError::Identity(IdentityError::Unauthorized | IdentityError::Forbidden) => "请先在电脑登录并生成关联口令，然后发送 /绑定 口令；也请检查账号或实例是否已撤销。",
                    GatewayError::Identity(IdentityError::RateLimited) => "请求过于频繁，请稍后重试。",
                    GatewayError::Busy | GatewayError::State(StateError::Busy) => "当前任务仍在运行或执行状态未确认；请先等待完成或发送 /取消。",
                    GatewayError::Connection => "无法建立工作区连接；请检查执行器、SSH 连接，以及路径是否是实例上存在且可访问的目录。",
                    GatewayError::Invalid => "参数不正确。发送 /help 查看命令，或发送“菜单”打开操作卡片。",
                    GatewayError::StaleInteraction => "卡片已过期、已使用或会话已改变。提问可发送 /问题 重开，其他操作发送“菜单”。",
                    GatewayError::State(StateError::InvocationConflict) => "内容已被另一处修改，本次确认未生效。请查看最新内容后重新操作。",
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
        let mut normalized = message.clone();
        normalized.text = commands::normalize(&message.text);
        if let Some(registry) = self.connector.command_registry() {
            if let Some(command) = registry
                .resolve(message.text.trim())
                .or_else(|| registry.resolve(&normalized.text))
            {
                if let Some(schema) = &command.entry.args_schema {
                    if crate::commands::parse_arguments(schema, &command.args).is_err() {
                        return Ok(Some(("命令参数不正确。发送 /help 查看用法。".into(), None)));
                    }
                }
                normalized.text = if command.args.is_empty() {
                    format!("/{}", command.entry.fullname)
                } else {
                    format!("/{} {}", command.entry.fullname, command.args)
                };
            }
        }
        let message = &normalized;
        let text = message.text.trim();
        if text == "/help" || text.starts_with("/help ") {
            return Ok(Some((
                self.command_help(text.strip_prefix("/help").unwrap().trim())
                    .into(),
                self.identity
                    .resolve_channel(&message.route.identity)
                    .await
                    .ok(),
            )));
        }
        if ["/new", "/compact", "/pwd"]
            .iter()
            .any(|command| text.starts_with(&format!("{command} ")))
        {
            return Ok(Some((
                "这些命令不接收参数。发送 /help 查看用法。".into(),
                None,
            )));
        }
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
        if let Some(body) = self.unavailable_command(text) {
            return Ok(Some((body.into(), None)));
        }
        let (principal, devices) = self
            .identity
            .channel_devices(&message.route.identity)
            .await?;
        let selected = self.store.selection(&message.route, principal).await?;
        if let Some(body) = self.access_command(message, principal).await? {
            return Ok(Some((body, Some(principal))));
        }
        if text == "/菜单详情" || text.starts_with("/菜单详情 ") {
            let arguments: Vec<_> = text
                .strip_prefix("/菜单详情")
                .unwrap_or_default()
                .split_whitespace()
                .collect();
            let (snapshot, page) = match arguments.as_slice() {
                [] => (None, 1),
                [page] => (
                    None,
                    page.parse::<usize>().map_err(|_| GatewayError::Invalid)?,
                ),
                [snapshot, page] => (
                    Some(Uuid::parse_str(snapshot).map_err(|_| GatewayError::Invalid)?),
                    page.parse::<usize>().map_err(|_| GatewayError::Invalid)?,
                ),
                _ => return Err(GatewayError::Invalid),
            };
            let body = match self
                .store
                .menu_snapshot(&message.route, principal, snapshot)
                .await?
            {
                Some(delivery) => {
                    let DeliveryBody::Menu { card } = delivery.body else {
                        return Err(GatewayError::Invalid);
                    };
                    let text = super::menu_view::page(&card, delivery.delivery_id, page)?.render();
                    format!("{text}\n\n操作这份菜单：/选 {} 编号\n直接 /选 编号 操作当前最新菜单。过期或已使用的选项需重开菜单。", delivery.delivery_id)
                }
                None => "这个聊天没有可查看的菜单。发送“菜单”重新打开。".into(),
            };
            return Ok(Some((body.into(), Some(principal))));
        }
        if let Some(body) = self.interaction_command(message, text, principal).await? {
            return Ok(Some((body, Some(principal))));
        }
        if let Some(body) = self
            .personal_command(message, text, principal, selected)
            .await?
        {
            return Ok(Some((body, Some(principal))));
        }
        if text == "/结果" || text.starts_with("/结果 ") {
            let arguments: Vec<_> = text
                .strip_prefix("/结果")
                .unwrap_or_default()
                .split_whitespace()
                .collect();
            let (turn, page) = match arguments.as_slice() {
                [] => (None, 1),
                [page] => (
                    None,
                    page.parse::<usize>().map_err(|_| GatewayError::Invalid)?,
                ),
                [turn, page] => (
                    Some(Uuid::parse_str(turn).map_err(|_| GatewayError::Invalid)?),
                    page.parse::<usize>().map_err(|_| GatewayError::Invalid)?,
                ),
                _ => return Err(GatewayError::Invalid),
            };
            let body = match self
                .saved_result(&message.route, principal, page, turn)
                .await?
            {
                Some(result) => result.text(),
                None => "这个聊天还没有已保存的任务结果。任务完成后可发送 /结果 查看。".into(),
            };
            return Ok(Some((body.into(), Some(principal))));
        }
        let action = match text {
            "/菜单" | "菜单" | "/帮助" => Some(super::menu_store::MenuAction::Home),
            "/设备" | "设备" | "/实例" | "实例" | "/电脑" => {
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
                    "已连接设备：{}\n工作区：{}\n可直接发送任务，或发送“菜单”选择目录与权限。",
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
        if text == "/new" {
            let body = self
                .new_chat_session(&message.route, principal, device, session)
                .await?;
            return Ok(Some((body.into(), Some(principal))));
        }
        if text == "/pwd" {
            return Ok(Some((
                format!(
                    "当前会话：{}\n设备：{}\n目录：{}\n权限：{}",
                    session,
                    device.name,
                    state.frozen.binding.workspace,
                    permission_label(state.permissions()?)
                )
                .into(),
                Some(principal),
            )));
        }
        if text == "/threads" || text.starts_with("/threads ") || text.starts_with("/resume ") {
            let body = self
                .history_control(&message.route, principal, session, text)
                .await?;
            return Ok(Some((body.into(), Some(principal))));
        }
        if matches!(text, "/compact" | "/core:compact") && state.history.is_empty() {
            return Ok(Some((
                "当前会话没有历史可压缩，没有调用模型。".into(),
                Some(principal),
            )));
        }
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
            return Ok(Some((
                self.activity(&message.route, principal, device, &state)
                    .await?
                    .text()
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
        if text == "/问题"
            || text == "/回答"
            || text.starts_with("/回答 ")
            || !text.starts_with('/')
        {
            if let Some(request) = self.hub.current_question(&message.route).await? {
                let card = if matches!(text, "/问题" | "/回答") {
                    self.hub.question_menu(&message.route, request).await?
                } else {
                    if let Some(origin) = self.origins.read().get(&session) {
                        *origin.write() = Some(SessionOrigin::Platform(message.event_id.clone()));
                    }
                    self.hub
                        .question_text(
                            &message.route,
                            request,
                            text.strip_prefix("/回答 ").unwrap_or(text),
                        )
                        .await?
                };
                return Ok(Some((DeliveryBody::Menu { card }, Some(principal))));
            }
            if text.starts_with("/回答") || text == "/问题" {
                return Ok(Some((
                    "当前没有等待回答的问题。可直接发送任务，或用 /状态 查看进度。".into(),
                    Some(principal),
                )));
            }
        }
        if text.starts_with('/')
            && !self
                .connector
                .command_registry()
                .is_some_and(|r| r.resolve(text).is_some())
        {
            return Ok(Some((
                "未知命令或参数不正确。发送 /help 查看完整用法；本次没有调用 Agent。".into(),
                Some(principal),
            )));
        }
        self.ensure_task_session(&message.route, principal, device, &state)
            .await?;
        let origin = self
            .origins
            .read()
            .get(&session)
            .cloned()
            .ok_or(GatewayError::RecoveryRequired)?;
        *origin.write() = Some(SessionOrigin::Platform(message.event_id.clone()));
        self.store
            .stage(&message.key()?, principal, session)
            .await?;
        let turn = self
            .submit_session_task(
                principal,
                session,
                format!("gateway:{}", message.key()?),
                MessageContent::Text(message.text.clone()),
            )
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
        if !self.route_allowed(route).await? {
            return Err(IdentityError::Forbidden.into());
        }
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
            let _policy_read = self.access_gate.read().await;
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
                DeliveryBody::Menu { card } => match card.question_request {
                    Some((request, revision)) => {
                        self.hub
                            .question_delivery_current(&pending.delivery.route, request, revision)
                            .await?
                    }
                    None => true,
                },
                _ => true,
            };
            if !authorized || !live || !self.route_allowed(&pending.delivery.route).await? {
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
