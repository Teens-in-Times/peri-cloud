use super::*;
use async_trait::async_trait;
use peri_agent::{
    middleware::r#trait::Middleware,
    tools::{BaseTool, ToolContext},
};
use serde_json::{json, Value};

pub(crate) struct TaskTools {
    pub scope: TaskScope,
    pub turn: Uuid,
}
#[async_trait]
impl Middleware for TaskTools {
    fn name(&self) -> &str {
        "CloudTaskTools"
    }
    fn collect_tools(&self, _: &str) -> Vec<Box<dyn BaseTool>> {
        let names: &[&str] = if self.scope.role == AgentRole::Worker {
            &["inspect_task", "update_task", "message_parent"]
        } else {
            &[
                "delegate_task",
                "list_execution_sessions",
                "list_tasks",
                "inspect_task",
                "message_task",
                "control_task",
            ]
        };
        names
            .iter()
            .map(|name| {
                Box::new(TaskTool {
                    scope: self.scope.clone(),
                    turn: self.turn,
                    name,
                }) as Box<dyn BaseTool>
            })
            .collect()
    }
}
struct TaskTool {
    scope: TaskScope,
    turn: Uuid,
    name: &'static str,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Inspect {
    task_id: Uuid,
    #[serde(default = "status_view")]
    view: String,
    #[serde(default)]
    after: i64,
    #[serde(default = "page_size")]
    limit: usize,
}
fn status_view() -> String {
    "status".into()
}
fn page_size() -> usize {
    12
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendMessage {
    task_id: Uuid,
    body: String,
    expected_revision: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Control {
    task_id: Uuid,
    expected_revision: u64,
    action: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    task_id: Uuid,
    expected_revision: u64,
    note: String,
    #[serde(default)]
    work_plan: Option<Vec<WorkItem>>,
    #[serde(default)]
    applied_messages: Vec<i64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Parent {
    body: String,
    #[serde(default)]
    wait_for_reply: bool,
}

#[async_trait]
impl BaseTool for TaskTool {
    fn name(&self) -> &str {
        self.name
    }
    fn is_direct(&self) -> bool {
        true
    }
    fn description(&self) -> &str {
        match self.name {
        "delegate_task"=>"Persist an independent cloud execution task and return immediately. Supply goal, constraints and acceptance; optional device/workspace must belong to this user. Native tools remain available for small operations. Workers use their own context and the original tool approval policy.",
        "list_execution_sessions"=>"List authorized execution conversations for explicit reuse. Reuse preserves work context across independent tasks, within the same device, canonical workspace and privacy scope. Supply the execution_session plus matching device/workspace to delegate_task.",
        "list_tasks"=>"List bounded task status for this logical conversation. Runtime state and worker-authored notes are separate. Never poll continuously while the user wants to chat.",
        "inspect_task"=>"Read status, bounded trace, retained conversation evidence or task messages. Trace previews are cropped; conversation view reads retained canonical content with exclusions respected. Does not expose private reasoning.",
        "message_task"=>"Send a supplement or question to this conversation's task at its current revision. Submitted, consumed and applied are different receipts. A message cannot authorize additional permissions.",
        "control_task"=>"Request cancellation of a task at its current revision. A cancellation request is not proof that an already dispatched effect stopped. Unknown execution is never replayed.",
        "update_task"=>"Update this execution task's work plan, discoveries and blockers, and explicitly acknowledge applied parent messages. These are worker-authored claims; they do not overwrite host lifecycle state. Trace is collected automatically.",
        _=>"Send a message or question to the main assistant through its durable conversation queue. For a question requiring a response, set wait_for_reply=true; the worker waits asynchronously while the main assistant remains free to chat. Do not send ordinary user messages directly.",
    }
    }
    fn parameters(&self) -> Value {
        let id = json!({"type":"string","format":"uuid"});
        let rev = json!({"type":"integer","minimum":0});
        let (properties, required) = match self.name {
            "delegate_task" => (
                json!({"goal":{"type":"string","maxLength":16384},"constraints":{"type":"string","maxLength":16384},"acceptance":{"type":"string","maxLength":8192},"device_id":id,"workspace":{"type":"string"},"execution_session":id}),
                vec!["goal"],
            ),
            "list_tasks" | "list_execution_sessions" => (json!({}), vec![]),
            "inspect_task" => (
                json!({"task_id":id,"view":{"type":"string","enum":["status","trace","conversation","messages","operations"]},"after":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":32}}),
                vec!["task_id"],
            ),
            "message_task" => (
                json!({"task_id":id,"expected_revision":rev,"body":{"type":"string","maxLength":16384}}),
                vec!["task_id", "expected_revision", "body"],
            ),
            "control_task" => (
                json!({"task_id":id,"expected_revision":rev,"action":{"type":"string","enum":["cancel"]}}),
                vec!["task_id", "expected_revision", "action"],
            ),
            "update_task" => (
                json!({"task_id":id,"expected_revision":rev,"note":{"type":"string","maxLength":4000},"work_plan":{"type":"array","maxItems":16,"items":{"type":"object","properties":{"name":{"type":"string"},"state":{"type":"string","enum":["pending","in_progress","done","blocked"]}},"required":["name","state"],"additionalProperties":false}},"applied_messages":{"type":"array","items":{"type":"integer"},"maxItems":16}}),
                vec!["task_id", "expected_revision", "note"],
            ),
            _ => (
                json!({"body":{"type":"string","maxLength":16384},"wait_for_reply":{"type":"boolean"}}),
                vec!["body"],
            ),
        };
        json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
    }
    async fn invoke(
        &self,
        input: Value,
        ctx: ToolContext<'_>,
    ) -> std::result::Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let service = self.scope.service.upgrade().ok_or(TaskError::Unavailable)?;
        if ctx.session_id.as_deref() != Some(self.scope.session.to_string().as_str()) {
            return Err(TaskError::Forbidden.into());
        }
        let call = ctx.tool_call_id.as_ref().ok_or(TaskError::Invalid)?;
        let key = format!("{}:{call}", self.turn);
        let result = match self.name {
            "delegate_task" => {
                let record = service
                    .delegate(&self.scope, self.turn, &key, serde_json::from_value(input)?)
                    .await?;
                service.summary(&record)
            }
            "list_tasks" | "list_execution_sessions" => {
                if input != json!({}) {
                    return Err(TaskError::Invalid.into());
                }
                if self.name == "list_execution_sessions" {
                    service.list_execution_sessions(&self.scope).await?
                } else {
                    service.list(&self.scope).await?
                }
            }
            "inspect_task" => {
                let args: Inspect = serde_json::from_value(input)?;
                let record = service.authorized(&self.scope, args.task_id).await?;
                match args.view.as_str() {
                    "status" => service.summary(&record),
                    "operations" => {
                        service
                            .operations(record.id, args.after, args.limit)
                            .await?
                    }
                    "trace" => {
                        service
                            .store
                            .trace(record.id, args.after, args.limit)
                            .await?
                    }
                    "conversation" => {
                        service
                            .conversation(&record, args.after, args.limit)
                            .await?
                    }
                    "messages" => {
                        let rows=sqlx::query_as::<_,(i64,String,String,String)>("SELECT seq,direction,body,state FROM agent_task_messages WHERE task_id=? AND seq>? ORDER BY seq LIMIT ?").bind(record.id.to_string()).bind(args.after).bind(args.limit.clamp(1,16) as i64).fetch_all(&service.store.pool).await?;
                        let mut messages = Vec::new();
                        let mut used = 0;
                        let mut consumed = Vec::new();
                        let count = rows.len();
                        for (id, direction, body, state) in rows {
                            let entry = json!({"message_id":id,"direction":direction,"body":body,"state":state});
                            let bytes = serde_json::to_vec(&entry)?.len();
                            if used + bytes > 24 * 1024 {
                                break;
                            }
                            used += bytes;
                            if self.scope.task == Some(record.id)
                                && direction == "parent"
                                && state == "submitted"
                            {
                                consumed.push(id);
                            }
                            messages.push(entry);
                        }
                        service.store.consume(record.id, &consumed).await?;
                        json!({"messages":messages,"next_cursor":messages.last().and_then(|m|m["message_id"].as_i64()).unwrap_or(args.after),"truncated":messages.len()<count,"full_bodies":true})
                    }
                    _ => return Err(TaskError::Invalid.into()),
                }
            }
            "message_task" => {
                if self.scope.role != AgentRole::Main {
                    return Err(TaskError::Forbidden.into());
                }
                let args: SendMessage = serde_json::from_value(input)?;
                let record = service.authorized(&self.scope, args.task_id).await?;
                if record.revision != args.expected_revision || record.terminal() {
                    return Err(TaskError::Conflict.into());
                }
                let receipt = service
                    .store
                    .message(record.id, &key, "parent", &args.body)
                    .await?;
                service.message_ready(record.id);
                receipt
            }
            "control_task" => {
                let args: Control = serde_json::from_value(input)?;
                let record = service.authorized(&self.scope, args.task_id).await?;
                if args.action != "cancel" || self.scope.role != AgentRole::Main {
                    return Err(TaskError::Invalid.into());
                }
                if record.revision != args.expected_revision {
                    return Err(TaskError::Conflict.into());
                }
                let record = service
                    .request_cancel(&self.scope, record.id, args.expected_revision)
                    .await?;
                service.summary(&record)
            }
            "update_task" => {
                let args: Update = serde_json::from_value(input)?;
                let mut record = service.authorized(&self.scope, args.task_id).await?;
                if self.scope.task != Some(record.id) || record.revision != args.expected_revision {
                    return Err(TaskError::Conflict.into());
                }
                if args.note.len() > 4000
                    || args.work_plan.as_ref().is_some_and(|plan| plan.len() > 16)
                    || args.work_plan.iter().flatten().any(|i| {
                        i.name.len() > 400
                            || !matches!(
                                i.state.as_str(),
                                "pending" | "in_progress" | "done" | "blocked"
                            )
                    })
                    || args.applied_messages.len() > 16
                {
                    return Err(TaskError::Invalid.into());
                }
                record.note = args.note;
                if let Some(plan) = args.work_plan {
                    record.plan = plan;
                }
                service.store.save(&mut record).await?;
                for id in args.applied_messages {
                    sqlx::query("UPDATE agent_task_messages SET state='applied' WHERE task_id=? AND seq=? AND direction='parent' AND state='consumed'").bind(record.id.to_string()).bind(id).execute(&service.store.pool).await?;
                }
                service.summary(&record)
            }
            _ => {
                let args: Parent = serde_json::from_value(input)?;
                let task = self.scope.task.ok_or(TaskError::Forbidden)?;
                let record = service.authorized(&self.scope, task).await?;
                let receipt = service
                    .store
                    .message(task, &key, "worker", &args.body)
                    .await?;
                service
                    .gateway
                    .upgrade()
                    .ok_or(TaskError::Unavailable)?
                    .notify_worker_message(
                        &record,
                        receipt["message_id"].as_i64().ok_or(TaskError::Invalid)?,
                        &args.body,
                    )
                    .await
                    .map_err(|_| TaskError::Unavailable)?;
                if args.wait_for_reply {
                    service
                        .wait_for_parent(&self.scope, &ctx.cancellation)
                        .await?
                } else {
                    receipt
                }
            }
        };
        Ok(serde_json::to_string(&result)?)
    }
}
