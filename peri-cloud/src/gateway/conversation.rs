//! Device-free cloud conversations use the same journal, Runtime and policy.
use super::*;
use crate::state::SessionState;

impl Gateway {
    pub(super) async fn route_question_input(
        &self,
        message: &InboundMessage,
        session: Uuid,
    ) -> GatewayResult<Option<DeliveryBody>> {
        let text = message.text.trim();
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
                return Ok(Some(DeliveryBody::Menu { card }));
            }
            if text.starts_with("/回答") || text == "/问题" {
                return Ok(Some(
                    "当前没有等待回答的问题。可直接发送任务，或用 /状态 查看进度。".into(),
                ));
            }
        }
        Ok(None)
    }

    pub(super) async fn ensure_cloud_conversation(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
    ) -> GatewayResult<Uuid> {
        if let Some(session) = self.store.selection(route, principal).await? {
            return Ok(session);
        }
        if !self.route_allowed(route).await?
            || self.identity.resolve_channel(&route.identity).await? != principal
        {
            return Err(IdentityError::Forbidden.into());
        }
        let agent = self.connector.new_conversation(principal).await?;
        let frozen = agent.frozen_session(principal);
        if frozen.device_id.is_some() || frozen.binding.is_some() {
            return Err(StateError::BindingConflict.into());
        }
        self.attach_conversation(route, principal, agent).await?;
        self.store
            .select(route, principal, frozen.session_id)
            .await?;
        Ok(frozen.session_id)
    }
    pub(super) async fn new_cloud_conversation(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
        old: Uuid,
    ) -> GatewayResult<Uuid> {
        let state = self.journal().session(principal, old).await?;
        let agent = self.connector.new_conversation(principal).await?;
        let id = agent.frozen_session(principal).session_id;
        self.attach_conversation(route, principal, agent).await?;
        self.journal()
            .set_permissions(principal, id, state.permissions()?)
            .await?;
        self.inherit_model_selection(principal, old, id).await?;
        self.store.select(route, principal, id).await?;
        Ok(id)
    }
    pub(super) async fn route_cloud_message(
        &self,
        message: &InboundMessage,
        principal: Uuid,
        state: SessionState,
    ) -> GatewayResult<Option<(DeliveryBody, Option<Uuid>)>> {
        let text = message.text.trim();
        let session = state.frozen.session_id;
        let reply = match text {
            "/new" => Some(format!(
                "已开始空白云端会话：{}。原历史保留。",
                self.new_cloud_conversation(&message.route, principal, session)
                    .await?
            )),
            "/pwd" => Some(format!(
                "当前会话：{session}\n云端对话，无设备工具。需要处理文件时可通过“实例”选择设备。"
            )),
            "/状态" => Some(
                match self.journal().active_turn(principal, session).await? {
                    Some(turn) => format!(
                        "云端对话：{session}\n运行状态：{:?}\n任务：{}",
                        turn.state, turn.turn_id
                    ),
                    None => "云端对话空闲，可以直接聊天。".into(),
                },
            ),
            "/取消" => Some(
                if let Some(turn) = self.journal().active_turn(principal, session).await? {
                    self.runtime
                        .cancel(principal, session, turn.turn_id)
                        .await?;
                    "已提交取消请求。".into()
                } else {
                    "没有正在运行的云端任务。".into()
                },
            ),
            _ => None,
        };
        if let Some(reply) = reply {
            return Ok(Some((reply.into(), Some(principal))));
        }
        if let Some(mode) = text.strip_prefix("/权限 ") {
            let mode = match mode.trim() {
                "默认" => PermissionMode::Default,
                "编辑" => PermissionMode::AcceptEdit,
                "全部" => PermissionMode::Bypass,
                _ => return Err(GatewayError::Invalid),
            };
            self.journal()
                .set_permissions(principal, session, mode)
                .await?;
            return Ok(Some((
                format!("当前会话权限已设为{}。", permission_label(mode)).into(),
                Some(principal),
            )));
        }
        if text == "/threads" || text.starts_with("/threads ") || text.starts_with("/resume ") {
            return Ok(Some((
                self.history_control(&message.route, principal, session, text)
                    .await?
                    .into(),
                Some(principal),
            )));
        }
        if text.starts_with('/')
            && !self
                .connector
                .command_registry()
                .is_some_and(|r| r.resolve(text).is_some())
        {
            return Ok(Some((
                "此命令在云端对话中不可用。发送 /help 查看命令；“实例”可选择设备。".into(),
                Some(principal),
            )));
        }
        self.enqueue_session_message(message, principal, session)
            .await?;
        Ok(None)
    }
}
