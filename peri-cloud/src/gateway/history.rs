//! Owned history and idle resume; no caller-supplied identity or tool replay.
use super::*;
use crate::state::SessionState;
use sqlx::Row;

impl Gateway {
    pub(super) fn unavailable_command(&self, text: &str) -> Option<String> {
        let verb = text.split_whitespace().next()?.trim_start_matches('/');
        if self.connector.command_registry()?.resolve(text).is_some() {
            return None;
        }
        crate::commands::catalog(self.connector.command_registry()?.as_ref())
            .into_iter()
            .find(|d| {
                d.availability == "unavailable"
                    && (d.name == verb || d.name == format!("ui:{verb}"))
            })
            .map(|d| {
                format!(
                    "/{}：{}。本次未执行，也没有交给模型。",
                    verb,
                    d.reason.unwrap_or("目标服务未接入")
                )
            })
    }
    pub(super) async fn history_control(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
        current: Uuid,
        text: &str,
    ) -> GatewayResult<String> {
        let (verb, args) = text.split_once(' ').unwrap_or((text, ""));
        if verb == "/resume" {
            let target = Uuid::parse_str(args).map_err(|_| GatewayError::Invalid)?;
            if self
                .store
                .selections(principal)
                .await?
                .iter()
                .any(|(other, id)| *id == target && other != route)
            {
                return Ok("这条会话仍由另一聊天入口使用。请在原聊天继续，或使用 /new 新建会话，避免审批和结果发往错误入口。".into());
            }
            let (_, devices) = self.identity.channel_devices(&route.identity).await?;
            let old = self.journal().session(principal, current).await?;
            let state = self.journal().session(principal, target).await?;
            if [current, target]
                .iter()
                .any(|id| self.runtime.status().active_sessions.contains(id))
                || !old.execution_settled()
                || !state.execution_settled()
                || self.journal().has_unconfirmed_turn(current).await?
                || self.journal().has_unconfirmed_turn(target).await?
            {
                return Err(GatewayError::Busy);
            }
            let device = devices
                .iter()
                .find(|d| d.id == state.frozen.device_id && !d.revoked)
                .ok_or(IdentityError::Forbidden)?;
            let agent = self
                .connector
                .open(principal, device, Some(&state.frozen))
                .await?;
            let origin = Arc::new(RwLock::new(None));
            let broker = Arc::new(SessionBroker {
                hub: self.hub.clone(),
                route: route.clone(),
                principal,
                session: target,
                device: device.id,
                device_name: device.name.clone(),
                workspace: state.frozen.binding.workspace.clone(),
                origin: origin.clone(),
            });
            self.runtime.rebind_idle(principal, agent, broker).await?;
            self.origins.write().insert(target, origin);
            self.store.select(route, principal, target).await?;
            return Ok(format!("已恢复会话：{target}\n设备：{}\n目录：{}\n沿用该会话已保存的上下文、权限和模型选择；不会重新执行历史工具。",device.name,state.frozen.binding.workspace));
        }
        if args == "messages" || args.starts_with("messages ") {
            let page = args.strip_prefix("messages").unwrap().trim();
            let page = if page.is_empty() {
                1
            } else {
                page.parse::<usize>().map_err(|_| GatewayError::Invalid)?
            };
            if page == 0 {
                return Err(GatewayError::Invalid);
            }
            let state = self.journal().session(principal, current).await?;
            let messages: Vec<_> = state
                .history
                .iter()
                .filter_map(|p| p.as_message())
                .filter(|m| matches!(m, peri_acp_types::messages::BaseMessage::Human { .. }))
                .collect();
            let mut output = format!("本会话用户消息 · 第 {page} 页\n");
            for message in messages.iter().skip((page - 1).saturating_mul(6)).take(6) {
                let preview = message.content().chars().take(80).collect::<String>();
                output.push_str(&format!("\n{}\n{}\n", message.id().as_uuid(), preview));
            }
            if page.saturating_mul(6) < messages.len() {
                output.push_str(&format!("\n下一页：/history messages {}", page + 1));
            }
            output.push_str("\n仅回退上下文：/rewind 消息ID --no-revert-files");
            return Ok(output);
        }
        let page = if args.is_empty() {
            1
        } else {
            args.parse::<usize>().map_err(|_| GatewayError::Invalid)?
        };
        if page == 0 {
            return Err(GatewayError::Invalid);
        }
        let rows=sqlx::query("SELECT state_json FROM cloud_sessions WHERE principal=? ORDER BY rowid DESC LIMIT 7 OFFSET ?")
            .bind(principal.to_string()).bind(i64::try_from((page-1).saturating_mul(6)).map_err(|_|GatewayError::Invalid)?).fetch_all(&self.store.pool).await?;
        let mut output = format!("我的历史会话 · 第 {page} 页\n");
        for row in rows.iter().take(6) {
            let state: SessionState = serde_json::from_str(row.get("state_json"))?;
            output.push_str(&format!(
                "\n{}{}\n设备：{}\n目录：{}\n",
                state.frozen.session_id,
                if state.frozen.session_id == current {
                    "（当前）"
                } else {
                    ""
                },
                state.frozen.device_id,
                state.frozen.binding.workspace
            ));
        }
        if rows.len() > 6 {
            output.push_str(&format!("\n下一页：/history {}", page + 1));
        }
        output.push_str("\n恢复：/resume 会话ID\n消息 ID：/history messages");
        Ok(output)
    }
}
