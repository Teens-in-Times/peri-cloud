//! QQ and the portal project the same recorded conversation facts. No remote
//! connection, model call or execution is needed to view status or saved output.
use serde::Serialize;
use sqlx::Row;
use uuid::Uuid;

use super::Gateway;
use crate::gateway::{ChannelRoute, GatewayError, GatewayResult};
use crate::identity::{Authenticated, DeviceRecord, IdentityError};
use crate::state::{SessionState, TurnState};

pub(crate) const RESULT_PAGE_CHARS: usize = 1200;

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityPhase {
    Idle,
    Running,
    WaitingApproval,
    WaitingAnswer,
    Cancelling,
    NeedsAttention,
}

impl ActivityPhase {
    fn label(self) -> &'static str {
        match self {
            Self::Idle => "可以继续对话",
            Self::Running => "任务执行中",
            Self::WaitingApproval => "等待你审批操作",
            Self::WaitingAnswer => "等待你回答问题",
            Self::Cancelling => "正在取消，尚未确认停止",
            Self::NeedsAttention => "执行结果待核实，请先查看任务状态",
        }
    }
}

#[derive(Serialize)]
pub struct ConversationActivity {
    pub adapter: String,
    pub session_id: Uuid,
    pub device_id: Uuid,
    pub device_name: String,
    pub conversation_id: String,
    pub workspace: String,
    pub permission: String,
    pub phase: ActivityPhase,
    pub label: String,
    pub turn_id: Option<Uuid>,
    pub waiting_count: usize,
    pub has_result: bool,
    pub next_model: Option<String>,
    pub turn_model: Option<String>,
}

impl ConversationActivity {
    pub(super) fn text(&self) -> String {
        format!(
            "执行设备：{}\n工作目录：{}\n权限：{}\n状态：{}{}",
            self.device_name,
            self.workspace,
            self.permission,
            self.label,
            if self.has_result {
                "\n查看已保存结果：/结果"
            } else {
                ""
            }
        )
    }
}

#[derive(Serialize)]
pub struct SavedResultPage {
    pub session_id: Uuid,
    pub turn_id: Uuid,
    pub device_name: String,
    pub workspace: String,
    pub page: usize,
    pub page_count: usize,
    pub text: String,
    pub turn_state: TurnState,
}

impl SavedResultPage {
    pub(super) fn text(&self) -> String {
        let next = if self.page < self.page_count {
            format!("\n下一页：/结果 {} {}", self.turn_id, self.page + 1)
        } else {
            String::new()
        };
        format!(
            "已保存结果 · {} · {}/{} 页\n{}\n\n{}{}\n仅查看已有结果，不重新执行任务。",
            display_excerpt(&self.device_name, 50),
            self.page,
            self.page_count,
            display_excerpt(&self.workspace, 120),
            self.text,
            next
        )
    }
}

impl Gateway {
    pub(super) async fn activity(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
        device: &DeviceRecord,
        state: &SessionState,
    ) -> GatewayResult<ConversationActivity> {
        let session = state.frozen.session_id;
        let active = self
            .runtime
            .journal()
            .active_turn(principal, session)
            .await?;
        let cards: Vec<_> = self
            .hub
            .browser_cards(principal)
            .await?
            .into_iter()
            .filter(|c| c.session_id == session)
            .collect();
        let phase = match active.as_ref() {
            Some(turn) if turn.state == TurnState::RecoveryRequired => {
                ActivityPhase::NeedsAttention
            }
            Some(turn) if turn.cancel_requested => ActivityPhase::Cancelling,
            Some(_)
                if cards.iter().any(|c| {
                    matches!(
                        c.context,
                        peri_acp_types::interaction::InteractionContext::Questions { .. }
                    )
                }) =>
            {
                ActivityPhase::WaitingAnswer
            }
            Some(_) if !cards.is_empty() => ActivityPhase::WaitingApproval,
            Some(_) => ActivityPhase::Running,
            None if !state.execution_settled() => ActivityPhase::NeedsAttention,
            None => ActivityPhase::Idle,
        };
        let next_model = if let Some(models) = self.connector.account_models() {
            if let Some(aggregate) = models.aggregation() {
                aggregate.current_model(principal, Some(session)).await.ok()
            } else {
                models
                    .choice(principal)
                    .await
                    .ok()
                    .flatten()
                    .map(|c| c.model)
                    .or_else(|| Some(models.default_model().to_owned()))
            }
        } else {
            None
        };
        let turn_model = if let Some(turn) = active.as_ref() {
            sqlx::query_scalar::<_, String>(
                "SELECT model FROM turn_models WHERE turn_id=? AND session_id=? AND principal=?",
            )
            .bind(turn.turn_id.to_string())
            .bind(session.to_string())
            .bind(principal.to_string())
            .fetch_optional(&self.store.pool)
            .await?
        } else {
            None
        };
        Ok(ConversationActivity {
            adapter: route.identity.adapter_instance_id.clone(),
            session_id: session,
            device_id: device.id,
            device_name: device.name.clone(),
            conversation_id: route.conversation_id.clone(),
            workspace: state.frozen.binding.workspace.clone(),
            permission: super::permission_label(state.permissions()?).into(),
            phase,
            label: phase.label().into(),
            turn_id: active.map(|t| t.turn_id),
            waiting_count: cards.len(),
            has_result: self.latest_result(route, principal).await?.is_some(),
            next_model,
            turn_model,
        })
    }

    async fn latest_result(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
    ) -> GatewayResult<Option<(Uuid, Uuid)>> {
        self.result_reference(route, principal, None).await
    }

    async fn result_reference(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
        turn: Option<Uuid>,
    ) -> GatewayResult<Option<(Uuid, Uuid)>> {
        let turn = turn.map(|t| t.to_string());
        let row = sqlx::query("WITH sources AS (SELECT principal,route_json,session_id,turn_id FROM gateway_inbox UNION ALL SELECT o.principal,json_extract(o.result_json,'$.route_json'),o.target,t.id FROM control_operations o JOIN cloud_turns t ON t.session_id=o.target AND t.request_key='web:'||o.id WHERE o.kind='task.submit') SELECT i.session_id,i.turn_id FROM sources i JOIN cloud_turns t ON t.id=i.turn_id AND t.session_id=i.session_id JOIN cloud_sessions s ON s.id=i.session_id WHERE i.principal=? AND s.principal=? AND i.route_json=? AND t.state NOT IN ('queued','running') AND (? IS NULL OR i.turn_id=?) ORDER BY t.rowid DESC LIMIT 1")
            .bind(principal.to_string()).bind(principal.to_string()).bind(serde_json::to_string(route)?)
            .bind(&turn).bind(&turn)
            .fetch_optional(&self.store.pool).await?;
        row.map(|row| {
            Ok((
                Uuid::parse_str(row.get("session_id"))
                    .map_err(|_| GatewayError::RecoveryRequired)?,
                Uuid::parse_str(row.get("turn_id")).map_err(|_| GatewayError::RecoveryRequired)?,
            ))
        })
        .transpose()
    }

    pub(super) async fn saved_result(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
        page: usize,
        turn: Option<Uuid>,
    ) -> GatewayResult<Option<SavedResultPage>> {
        if page == 0 {
            return Err(GatewayError::Invalid);
        }
        let (owner, devices) = self.identity.channel_devices(&route.identity).await?;
        if owner != principal {
            return Err(IdentityError::Forbidden.into());
        }
        let Some((session, turn)) = self.result_reference(route, principal, turn).await? else {
            return Ok(None);
        };
        let state = self.runtime.journal().session(principal, session).await?;
        let device = devices
            .iter()
            .find(|d| d.id == state.frozen.device_id && !d.revoked)
            .ok_or(IdentityError::Forbidden)?;
        let turn = self
            .runtime
            .journal()
            .turn(principal, session, turn)
            .await?;
        let text = turn
            .replies
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let (text, page_count) = page_text(&text, page)?;
        Ok(Some(SavedResultPage {
            session_id: session,
            turn_id: turn.turn_id,
            device_name: device.name.clone(),
            workspace: state.frozen.binding.workspace,
            page,
            page_count,
            text,
            turn_state: turn.state,
        }))
    }

    pub async fn browser_activity(
        &self,
        actor: &Authenticated,
    ) -> GatewayResult<Vec<ConversationActivity>> {
        self.identity.browser_scope(actor).await?;
        let principal = actor.principal().id;
        let mut result = Vec::new();
        for (route, session) in self.store.selections(principal).await? {
            let (owner, devices) = match self.identity.channel_devices(&route.identity).await {
                Ok(value) => value,
                Err(IdentityError::Unauthorized | IdentityError::Forbidden) => continue,
                Err(error) => return Err(error.into()),
            };
            if owner != principal {
                continue;
            }
            let state = self.runtime.journal().session(principal, session).await?;
            if let Some(device) = devices
                .iter()
                .find(|d| d.id == state.frozen.device_id && !d.revoked)
            {
                result.push(self.activity(&route, principal, device, &state).await?);
            }
        }
        Ok(result)
    }

    pub async fn browser_result(
        &self,
        actor: &Authenticated,
        current_session: Uuid,
        page: usize,
        turn: Option<Uuid>,
    ) -> GatewayResult<Option<SavedResultPage>> {
        self.identity.browser_scope(actor).await?;
        let principal = actor.principal().id;
        for (route, session) in self.store.selections(principal).await? {
            if session == current_session {
                return self.saved_result(&route, principal, page, turn).await;
            }
        }
        Err(IdentityError::Forbidden.into())
    }
}

fn page_text(text: &str, page: usize) -> GatewayResult<(String, usize)> {
    let pages = text.chars().count().div_ceil(RESULT_PAGE_CHARS).max(1);
    if page == 0 || page > pages {
        return Err(GatewayError::Invalid);
    }
    let body = if text.is_empty() {
        "这次任务没有已保存的文字结果。".into()
    } else {
        text.chars()
            .skip((page - 1) * RESULT_PAGE_CHARS)
            .take(RESULT_PAGE_CHARS)
            .collect()
    };
    Ok((body, pages))
}

pub(super) fn interaction_details(
    card: &crate::gateway::InteractionCard,
    page: usize,
) -> GatewayResult<String> {
    let context = serde_json::to_string_pretty(&card.context)?;
    let (text, count) = page_text(&context, page)?;
    let code = &card.request_id.simple().to_string()[..8];
    Ok(format!(
        "操作详情 · {page}/{count} 页\n{}\n{}\n\n{text}\n\n只查看，不授权。{}",
        display_excerpt(&card.device_name, 50),
        display_excerpt(&card.workspace, 120),
        if page < count {
            format!("下一页：/详情 {code} {}", page + 1)
        } else {
            String::new()
        }
    ))
}

fn display_excerpt(value: &str, limit: usize) -> String {
    let mut text: String = value
        .chars()
        .take(limit)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if value.chars().count() > limit {
        text.push('…');
    }
    text
}

pub(crate) fn reply_preview(reply: &str, turn: Uuid) -> String {
    if reply.chars().count() <= RESULT_PAGE_CHARS {
        return reply.into();
    }
    format!(
        "{}\n\n完整结果已保存，发送 /结果 {turn} 1 分页查看；不会重新执行任务。",
        reply.chars().take(RESULT_PAGE_CHARS).collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_pages_preserve_all_text_and_bound_invalid_requests() {
        let original = "结果🦀e\u{301}\n".repeat(900);
        let (_, count) = page_text(&original, 1).unwrap();
        let joined: String = (1..=count)
            .map(|p| page_text(&original, p).unwrap().0)
            .collect();
        assert_eq!(joined, original);
        for page in [0, count + 1, usize::MAX] {
            assert!(page_text(&original, page).is_err());
        }
        assert!(reply_preview(&original, Uuid::nil()).contains("/结果"));
        assert!(reply_preview(&original, Uuid::nil()).chars().count() < RESULT_PAGE_CHARS + 100);
    }
}
