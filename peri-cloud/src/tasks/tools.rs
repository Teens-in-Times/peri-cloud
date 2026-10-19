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
            &["inspect_task", "update_task", "message_parent", "wait_task"]
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
        let mut tools: Vec<Box<dyn BaseTool>> = names
            .iter()
            .map(|name| {
                Box::new(TaskTool {
                    scope: self.scope.clone(),
                    turn: self.turn,
                    name,
                }) as Box<dyn BaseTool>
            })
            .collect();
        tools.extend(super::deliverables::tools(&self.scope));
        if self.scope.role == AgentRole::Main {
            tools.extend(super::devices::tools(&self.scope, self.turn));
        }
        tools
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
    window_id: Option<Uuid>,
    detail: Option<super::evidence::Detail>,
    kind: Option<String>,
    query: Option<String>,
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
struct TaskPage {
    #[serde(default)]
    after: i64,
    #[serde(default = "page_size")]
    limit: usize,
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
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wait {
    reason: String,
    resume_after_seconds: Option<u64>,
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
        "list_tasks"=>"List paginated bounded task history for this logical conversation, newest first; pass next_cursor as after. Runtime state and worker-authored notes are separate. Never poll continuously while the user wants to chat.",
        "inspect_task"=>"Read status, full task contract, approval/operation receipts, windows, bounded trace, conversation or messages. related_tasks pages prior tasks in the same explicitly reused execution session; these are historical evidence and do not grant control over other tasks. Trace supports kind/query filters. Use detail with source/id/offset/chars/version for complete evidence in bounded versioned chunks; preserve the returned version across pages. For archived context, list windows then select window_id in conversation. Cropped previews and agent claims are not complete evidence; private reasoning is excluded.",
        "message_task"=>"Send a supplement or question to this conversation's task at its current revision. Submitted, consumed and applied are different receipts. A message cannot authorize additional permissions.",
        "control_task"=>"Control a task at its current revision: pause at a safe boundary, resume a durable wait/paused task, rotate its context window, or cancel. Unknown execution is queried before any continuation; cancellation does not prove that an effect stopped.",
        "wait_task"=>"Durably wait for an external condition. Supply reason and optional resume_after_seconds (up to one week). Host stops at a safe boundary and releases the loop slot; main assistant can resume explicitly.",
        "update_task"=>"Update this execution task's work plan, discoveries and blockers, and explicitly acknowledge applied parent messages. These are worker-authored claims; they do not overwrite host lifecycle state. Trace is collected automatically.",
        _=>"Send a message or question to the main assistant through its durable conversation queue. For a question requiring a response, set wait_for_reply=true; the host durably suspends this run and resumes on the parent response while the main assistant remains free to chat. Do not send ordinary user messages directly.",
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
            "list_execution_sessions" => (json!({}), vec![]),
            "list_tasks" => (
                json!({"after":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":32}}),
                vec![],
            ),
            "inspect_task" => (
                json!({"task_id":id,"view":{"type":"string","enum":["status","trace","conversation","messages","operations","windows","contract","approvals","detail","related_tasks"]},"window_id":id,"after":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":32},"kind":{"type":"string","maxLength":80},"query":{"type":"string","maxLength":200},"detail":{"type":"object","properties":{"source":{"type":"string","enum":["operation_input","operation_output","approval_input","conversation"]},"id":{"type":"string","maxLength":256},"offset":{"type":"integer","minimum":0},"chars":{"type":"integer","minimum":1,"maximum":16384},"version":{"type":"string","maxLength":80}},"required":["source","id"],"additionalProperties":false}}),
                vec!["task_id"],
            ),
            "message_task" => (
                json!({"task_id":id,"expected_revision":rev,"body":{"type":"string","maxLength":16384}}),
                vec!["task_id", "expected_revision", "body"],
            ),
            "control_task" => (
                json!({"task_id":id,"expected_revision":rev,"action":{"type":"string","enum":["cancel","pause","resume","rotate"]}}),
                vec!["task_id", "expected_revision", "action"],
            ),
            "update_task" => (
                json!({"task_id":id,"expected_revision":rev,"note":{"type":"string","maxLength":4000},"work_plan":{"type":"array","maxItems":16,"items":{"type":"object","properties":{"name":{"type":"string"},"state":{"type":"string","enum":["pending","in_progress","done","blocked"]}},"required":["name","state"],"additionalProperties":false}},"applied_messages":{"type":"array","items":{"type":"integer"},"maxItems":16}}),
                vec!["task_id", "expected_revision", "note"],
            ),
            "wait_task" => (
                json!({"reason":{"type":"string","maxLength":4000},"resume_after_seconds":{"type":"integer","minimum":0,"maximum":604800}}),
                vec!["reason"],
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
            "list_tasks" => {
                let args: TaskPage = serde_json::from_value(input)?;
                service
                    .task_history_page(&self.scope, None, args.after, args.limit)
                    .await?
            }
            "list_execution_sessions" => {
                if input != json!({}) {
                    return Err(TaskError::Invalid.into());
                }
                service.list_execution_sessions(&self.scope).await?
            }
            "inspect_task" => {
                let args: Inspect = serde_json::from_value(input)?;
                let record = service.readable(&self.scope, args.task_id).await?;
                match args.view.as_str() {
                    "related_tasks" => {
                        service
                            .task_history_page(
                                &self.scope,
                                Some(record.execution_session),
                                args.after,
                                args.limit.clamp(1, 32),
                            )
                            .await?
                    }
                    "status" => {
                        let mut status = service.summary(&record);
                        status["instruction_version"] =
                            json!(service.instruction_watermark(record.id).await?);
                        status
                    }
                    "approvals" => {
                        service
                            .approval_history(record.id, args.after, args.limit)
                            .await?
                    }
                    "detail" => {
                        service
                            .detail(
                                &record,
                                args.window_id,
                                args.detail.ok_or(TaskError::Invalid)?,
                            )
                            .await?
                    }
                    "contract" => {
                        json!({"goal":record.goal,"constraints":record.constraints,"acceptance":record.acceptance,"task_id":record.id,"run_number":record.run_number,"window_id":record.worker_session,"note":record.note,"note_source":"agent"})
                    }
                    "windows" => {
                        let rows:Vec<(i64,String)> = sqlx::query_as("SELECT window_number,cloud_session FROM agent_execution_windows WHERE execution_session=? AND window_number>=? ORDER BY window_number LIMIT ?")
                            .bind(record.execution_session.to_string()).bind(args.after.max(0)).bind(args.limit.clamp(1,16) as i64).fetch_all(&service.store.pool).await?;
                        let next = rows.last().map_or(args.after, |r| r.0 + 1);
                        let remaining:i64=sqlx::query_scalar("SELECT COUNT(*) FROM agent_execution_windows WHERE execution_session=? AND window_number>=?").bind(record.execution_session.to_string()).bind(next).fetch_one(&service.store.pool).await?;
                        json!({"windows":rows.iter().map(|(n,id)|json!({"window_number":n,"window_id":id})).collect::<Vec<_>>(),"next_cursor":next,"has_more":remaining>0,"complete":args.after==0&&remaining==0})
                    }
                    "operations" => {
                        service.reconcile_native().await?;
                        service
                            .operations(record.id, args.after, args.limit)
                            .await?
                    }
                    "trace" => {
                        service
                            .store
                            .trace(
                                record.id,
                                args.after,
                                args.limit,
                                args.kind.as_deref(),
                                args.query.as_deref(),
                            )
                            .await?
                    }
                    "conversation" => {
                        service
                            .conversation(&record, args.after, args.limit, args.window_id)
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
                            if self.scope.task == Some(record.id) && direction == "parent" {
                                consumed.push(id);
                            }
                            messages.push(entry);
                        }
                        service
                            .store
                            .consume(record.id, self.scope.session, &consumed)
                            .await?;
                        let next = messages
                            .last()
                            .and_then(|m| m["message_id"].as_i64())
                            .unwrap_or(args.after);
                        let remaining: i64 = sqlx::query_scalar(
                            "SELECT COUNT(*) FROM agent_task_messages WHERE task_id=? AND seq>?",
                        )
                        .bind(record.id.to_string())
                        .bind(next)
                        .fetch_one(&service.store.pool)
                        .await?;
                        json!({"messages":messages,"next_cursor":next,"has_more":remaining>0,"truncated":messages.len()<count||remaining>0,"full_bodies":true,"complete":args.after==0&&remaining==0})
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
                if record.revision != args.expected_revision
                    || (record.terminal() && record.state != "completed")
                {
                    return Err(TaskError::Conflict.into());
                }
                let receipt = service
                    .store
                    .message(record.id, &key, "parent", &args.body)
                    .await?;
                receipt
            }
            "control_task" => {
                let args: Control = serde_json::from_value(input)?;
                let record = service.authorized(&self.scope, args.task_id).await?;
                if self.scope.role != AgentRole::Main {
                    return Err(TaskError::Invalid.into());
                }
                if record.revision != args.expected_revision {
                    return Err(TaskError::Conflict.into());
                }
                let record = service
                    .request_control(&self.scope, record.id, args.expected_revision, &args.action)
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
            "wait_task" => {
                let args: Wait = serde_json::from_value(input)?;
                service
                    .wait_external(&self.scope, &args.reason, args.resume_after_seconds)
                    .await?
            }
            _ => {
                let args: Parent = serde_json::from_value(input)?;
                let task = self.scope.task.ok_or(TaskError::Forbidden)?;
                let record = service.authorized(&self.scope, task).await?;
                let receipt = service
                    .store
                    .message(task, &key, "worker", &args.body)
                    .await?;
                let waiting = if args.wait_for_reply {
                    Some(
                        service
                            .wait_for_parent(&self.scope, &ctx.cancellation)
                            .await?,
                    )
                } else {
                    None
                };
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
                sqlx::query("UPDATE agent_task_messages SET state='forwarded' WHERE task_id=? AND seq=? AND direction='worker' AND state='submitted'")
                    .bind(task.to_string()).bind(receipt["message_id"].as_i64()).execute(&service.store.pool).await?;
                waiting.unwrap_or(receipt)
            }
        };
        Ok(serde_json::to_string(&result)?)
    }
}
