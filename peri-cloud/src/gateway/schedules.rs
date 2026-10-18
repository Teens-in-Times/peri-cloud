//! One control service for HTTP, IM and the persistent scheduler worker.
use super::*;
use crate::identity::Authenticated;
use crate::scheduler::{
    NewSchedule, Notify, Schedule, ScheduleMode, ScheduleRun, ScheduleStore, Timing,
};
#[path = "schedule_worker.rs"]
pub(super) mod worker;

impl Gateway {
    pub async fn browser_contact_policy(
        &self,
        actor: &Authenticated,
    ) -> GatewayResult<crate::scheduler::ContactPolicy> {
        self.identity.browser_scope(actor).await?;
        Ok(ScheduleStore::new(self.journal())
            .contact_policy(actor.principal().id)
            .await?)
    }
    pub async fn edit_browser_schedule(
        &self,
        actor: &Authenticated,
        id: Uuid,
        input: crate::scheduler::ScheduleEdit,
    ) -> GatewayResult<Schedule> {
        self.identity.browser_scope(actor).await?;
        let gate = self.account_gate(actor.principal().id.to_string());
        let _guard = gate.lock().await;
        if self.owner.lock().closing {
            return Err(GatewayError::Closing);
        }
        Ok(ScheduleStore::new(self.journal())
            .edit(actor.principal().id, id, input, self.identity.now())
            .await?)
    }
    pub async fn update_browser_contact_policy(
        &self,
        actor: &Authenticated,
        policy: crate::scheduler::ContactPolicy,
    ) -> GatewayResult<crate::scheduler::ContactPolicy> {
        self.identity.browser_scope(actor).await?;
        let gate = self.account_gate(actor.principal().id.to_string());
        let _guard = gate.lock().await;
        Ok(ScheduleStore::new(self.journal())
            .save_contact_policy(actor.principal().id, policy)
            .await?)
    }
    pub async fn browser_schedules(&self, actor: &Authenticated) -> GatewayResult<Vec<Schedule>> {
        self.identity.browser_scope(actor).await?;
        Ok(ScheduleStore::new(self.journal())
            .list(actor.principal().id)
            .await?)
    }
    pub async fn create_browser_schedule(
        &self,
        actor: &Authenticated,
        input: NewSchedule,
    ) -> GatewayResult<Schedule> {
        self.identity.browser_scope(actor).await?;
        let principal = actor.principal().id;
        let gate = self.account_gate(principal.to_string());
        let _guard = gate.lock().await;
        if self.owner.lock().closing {
            return Err(GatewayError::Closing);
        }
        let route = self.browser_task_route(actor, input.session_id).await?;
        let state = self.journal().session(principal, input.session_id).await?;
        if input.mode == ScheduleMode::Task {
            self.identity
                .channel_device(&route.identity, state.frozen.device()?)
                .await?;
        }
        let push = self
            .adapters
            .get(&route.identity.adapter_instance_id)
            .is_some_and(|a| a.supports_proactive());
        Ok(ScheduleStore::new(self.journal())
            .create(principal, &route, input, self.identity.now(), push)
            .await?)
    }
    pub async fn change_browser_schedule(
        &self,
        actor: &Authenticated,
        id: Uuid,
        revision: i64,
        action: &str,
    ) -> GatewayResult<Schedule> {
        self.identity.browser_scope(actor).await?;
        let principal = actor.principal().id;
        let gate = self.account_gate(principal.to_string());
        let _guard = gate.lock().await;
        if self.owner.lock().closing {
            return Err(GatewayError::Closing);
        }
        Ok(ScheduleStore::new(self.journal())
            .change(principal, id, revision, action, self.identity.now())
            .await?)
    }
    pub async fn browser_schedule_runs(
        &self,
        actor: &Authenticated,
        id: Uuid,
    ) -> GatewayResult<Vec<ScheduleRun>> {
        self.identity.browser_scope(actor).await?;
        Ok(ScheduleStore::new(self.journal())
            .runs(actor.principal().id, id)
            .await?)
    }
    pub(super) async fn schedule_command(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
        session: Option<Uuid>,
        text: &str,
        key: String,
    ) -> GatewayResult<String> {
        let store = ScheduleStore::new(self.journal());
        let args = text.strip_prefix("/定时").unwrap_or_default().trim();
        if args.is_empty() || args == "列表" {
            let schedules = store.list(principal).await?;
            let mut text = String::from("定时任务（时间为 UTC）\n");
            for task in schedules {
                text.push_str(&format!(
                    "\n{}\n{} · {} · 下次 {}\n设备 {}\n目录 {}\n版本 {} · {}\n",
                    task.title,
                    task.id,
                    if task.enabled {
                        "启用"
                    } else {
                        "暂停/已结束"
                    },
                    task.next_fire
                        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                        .map(|t| t.to_rfc3339())
                        .unwrap_or_else(|| "无".into()),
                    task.device_id
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| "聊天目标".into()),
                    task.workspace,
                    task.revision,
                    if task.notification == "push" {
                        "可主动通知"
                    } else {
                        "工作台收件箱；此入口不支持主动推送"
                    }
                ));
            }
            text.push_str("\n创建：/定时 一次 RFC3339时间 任务\n周期：/定时 周期 时区 五段cron | 任务\n例：/定时 周期 Asia/Shanghai 0 9 * * * | 汇总今天的待办\n管理：/定时 暂停|恢复|删除 UUID 版本号\n记录：/定时 记录 UUID\n暂停/删除不取消已开始的任务，停止执行用 /取消。主动聊天跟随此聊天的当前会话；设备任务冻结原会话、设备和目录。\n完整列表与参数可在工作台查看。");
            // Bound chat output; full account data remains available via authenticated API.
            if text.chars().count() > 10000 {
                return Ok("定时任务较多，请在工作台查看完整列表；管理命令：/定时 暂停|恢复|删除 UUID 版本号；/定时 记录 UUID。".into());
            }
            return Ok(text);
        }
        let words: Vec<_> = args.split_whitespace().collect();
        if let ["记录", id] = words.as_slice() {
            let id = Uuid::parse_str(id).map_err(|_| GatewayError::Invalid)?;
            let runs = store.runs(principal, id).await?;
            let lines: Vec<_> = runs
                .iter()
                .take(10)
                .map(|r| {
                    format!(
                        "{} · {} / 投递 {} · {}\n任务 {}",
                        r.scheduled_at,
                        r.state,
                        r.delivery_state,
                        r.reason.as_deref().unwrap_or(""),
                        r.turn_id
                            .map(|t| t.to_string())
                            .unwrap_or_else(|| "未执行".into())
                    )
                })
                .collect();
            return Ok(format!(
                "最近触发记录\n{}\n结果通过 /结果 或工作台查看。",
                lines.join("\n\n")
            ));
        }
        if let [action, id, revision] = words.as_slice() {
            let action = match *action {
                "暂停" => Some("pause"),
                "恢复" => Some("resume"),
                "删除" => Some("delete"),
                _ => None,
            };
            if let Some(action) = action {
                let task = store
                    .change(
                        principal,
                        Uuid::parse_str(id).map_err(|_| GatewayError::Invalid)?,
                        revision.parse().map_err(|_| GatewayError::Invalid)?,
                        action,
                        self.identity.now(),
                    )
                    .await?;
                return Ok(format!(
                    "已更新定时任务 {}，版本 {}。已开始的执行不受影响。",
                    task.id, task.revision
                ));
            }
        }
        let (timing, prompt) = if let Some(value) = args.strip_prefix("一次 ") {
            let (at, prompt) = value
                .split_once(char::is_whitespace)
                .ok_or(GatewayError::Invalid)?;
            (Timing::Once { at: at.into() }, prompt.trim())
        } else if let Some(value) = args.strip_prefix("周期 ") {
            let (schedule, prompt) = value.split_once('|').ok_or(GatewayError::Invalid)?;
            let (timezone, expression) = schedule
                .trim()
                .split_once(char::is_whitespace)
                .ok_or(GatewayError::Invalid)?;
            (
                Timing::Cron {
                    expression: expression.trim().into(),
                    timezone: timezone.into(),
                },
                prompt.trim(),
            )
        } else {
            return Err(GatewayError::Invalid);
        };
        let session_id = match session {
            Some(session) => session,
            None => self.ensure_cloud_conversation(route, principal).await?,
        };
        let input = NewSchedule {
            mode: ScheduleMode::Conversation,
            notify: Notify::Always,
            request_id: crate::scheduler::tools::stable_id(principal, &key),
            session_id,
            title: prompt.chars().take(80).collect(),
            prompt: prompt.into(),
            timing,
        };
        let push = self
            .adapters
            .get(&route.identity.adapter_instance_id)
            .is_some_and(|a| a.supports_proactive());
        let task = store
            .create(principal, route, input, self.identity.now(), push)
            .await?;
        Ok(format!(
            "已创建定时任务 {}\n下次 UTC：{}\n绑定此聊天，纯聊天不要求电脑在线；沿用云端额度。{}",
            task.id,
            task.next_fire
                .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                .map(|t| t.to_rfc3339())
                .unwrap_or_default(),
            if push {
                ""
            } else {
                "\n此入口不支持主动推送，请在工作台查看结果和审批。"
            }
        ))
    }
}
