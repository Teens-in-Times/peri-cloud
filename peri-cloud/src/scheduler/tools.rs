use super::*;
use async_trait::async_trait;
use peri_agent::{
    middleware::r#trait::Middleware,
    tools::{BaseTool, ToolContext},
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

pub(crate) struct ScheduleTools(pub crate::personal::PersonalScope);
#[async_trait]
impl Middleware for ScheduleTools {
    fn name(&self) -> &str {
        "CloudScheduleMiddleware"
    }
    fn collect_tools(&self, _cwd: &str) -> Vec<Box<dyn BaseTool>> {
        [
            "schedule_create",
            "schedule_list",
            "schedule_update",
            "schedule_remove",
            "schedule_runs",
        ]
        .into_iter()
        .filter(|name| !self.0.scheduled || matches!(*name, "schedule_list" | "schedule_runs"))
        .map(|name| {
            Box::new(ScheduleTool {
                scope: self.0.clone(),
                name,
            }) as Box<dyn BaseTool>
        })
        .collect()
    }
}
struct ScheduleTool {
    scope: crate::personal::PersonalScope,
    name: &'static str,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    #[serde(default)]
    mode: ScheduleMode,
    #[serde(default)]
    notify: Notify,
    title: String,
    prompt: String,
    timing: Timing,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    id: Uuid,
    revision: i64,
    action: String,
    edit: Option<Edit>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Edit {
    title: String,
    prompt: String,
    timing: Timing,
    notify: Notify,
    enabled: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Runs {
    id: Uuid,
}

pub(crate) fn stable_id(scope: Uuid, key: &str) -> Uuid {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("schedule:{scope}:{key}"));
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    Uuid::from_bytes(bytes)
}
#[async_trait]
impl BaseTool for ScheduleTool {
    fn name(&self) -> &str {
        self.name
    }
    fn is_direct(&self) -> bool {
        true
    }
    fn description(&self) -> &str {
        match self.name {
            "schedule_create" => "Create a persistent scheduled Agent task ONLY when the user asks for future/recurring work. Conversation mode follows this authorized chat across device changes and needs no executor; task mode freezes the current device session and workspace. Use once with explicit RFC3339 offset, or five-field cron with IANA timezone. List first for current UTC time; clarify an unknown user timezone. Normal tool approvals and quotas still apply at execution. Never promise push on an inbox-only entry.",
            "schedule_list" => "List this account's schedules and current UTC time. Includes revision, next fire, frozen target and notification capability.",
            "schedule_update" | "schedule_remove" => "Pause, resume, edit title/prompt/timing, or delete this account's schedule using its latest revision. For edit provide all edit fields; its authorized target cannot change. Does not cancel already claimed/running work. Deleted schedules cannot be resumed.",
            _ => "Read the latest 100 firing receipts for this account's schedule. Skipped and unknown outcomes are never automatically replayed.",
        }
    }
    fn parameters(&self) -> Value {
        match self.name {
            "schedule_create" => {
                json!({"type":"object","properties":{"mode":{"enum":["conversation","task"],"description":"conversation (default): personal check-ins/greetings without executor tools; task: actual device work"},"notify":{"enum":["always","when_useful"],"description":"always for an explicitly requested greeting; when_useful lets the Agent stay silent"},"title":{"type":"string","maxLength":128},"prompt":{"type":"string","description":"Future task instruction, without secrets or management slash commands"},"timing":{"oneOf":[{"type":"object","properties":{"kind":{"const":"once"},"at":{"type":"string","description":"RFC3339 with explicit timezone offset"}},"required":["kind","at"],"additionalProperties":false},{"type":"object","properties":{"kind":{"const":"cron"},"expression":{"type":"string"},"timezone":{"type":"string","description":"IANA timezone, e.g. Asia/Shanghai"}},"required":["kind","expression","timezone"],"additionalProperties":false},{"type":"object","properties":{"kind":{"const":"after"},"seconds":{"type":"integer","minimum":1,"maximum":4294967295u64}},"required":["kind","seconds"],"additionalProperties":false},{"type":"object","properties":{"kind":{"const":"interval"},"anchor":{"type":"integer"},"every_seconds":{"type":"integer","minimum":1,"maximum":4294967295u64}},"required":["kind","anchor","every_seconds"],"additionalProperties":false}]}},"required":["title","prompt","timing"],"additionalProperties":false})
            }
            "schedule_update" | "schedule_remove" => {
                let mut schema = json!({"type":"object","properties":{"id":{"type":"string"},"revision":{"type":"integer"},"action":{"enum":["pause","resume","delete","edit"]},"edit":{"type":"object","properties":{"title":{"type":"string","maxLength":128},"prompt":{"type":"string","maxLength":8192},"timing":{"type":"object"},"notify":{"enum":["always","when_useful"]},"enabled":{"type":"boolean"}},"required":["title","prompt","timing","notify","enabled"],"additionalProperties":false}},"required":["id","revision","action"],"additionalProperties":false});
                if self.name == "schedule_remove" {
                    schema["properties"]["action"] = json!({"const":"delete"});
                }
                schema
            }
            "schedule_runs" => {
                json!({"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false})
            }
            _ => json!({"type":"object","properties":{},"additionalProperties":false}),
        }
    }
    async fn invoke(
        &self,
        input: Value,
        ctx: ToolContext<'_>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let store = ScheduleStore::new(&self.scope.journal);
        let now = chrono::Utc::now().timestamp();
        let value = match self.name {
            "schedule_create" => {
                let create: Create = serde_json::from_value(input)?;
                let invocation = ctx
                    .invocation_id
                    .as_ref()
                    .ok_or("missing trusted invocation identity")?;
                let rows: Vec<(String,)> = sqlx::query_as(
                    "SELECT route_json FROM gateway_routes WHERE principal=? AND session_id=?",
                )
                .bind(self.scope.principal.to_string())
                .bind(self.scope.session_id.to_string())
                .fetch_all(&store.pool)
                .await?;
                let [(route,)] = rows.as_slice() else {
                    return Err("session has no unique current route".into());
                };
                let input = NewSchedule {
                    request_id: stable_id(self.scope.session_id, invocation.as_ref()),
                    session_id: self.scope.session_id,
                    mode: create.mode,
                    notify: create.notify,
                    title: create.title,
                    prompt: create.prompt,
                    timing: create.timing,
                };
                json!(
                    store
                        .create(
                            self.scope.principal,
                            &serde_json::from_str(route)?,
                            input,
                            now,
                            self.scope.schedule_push
                        )
                        .await?
                )
            }
            "schedule_update" | "schedule_remove" => {
                let update: Update = serde_json::from_value(input)?;
                if self.name == "schedule_remove" && update.action != "delete" {
                    return Err("schedule_remove requires action=delete".into());
                }
                if update.action == "edit" {
                    let edit = update.edit.ok_or("edit fields are required")?;
                    let invocation = ctx
                        .invocation_id
                        .as_ref()
                        .ok_or("missing trusted invocation identity")?;
                    let edited = store
                        .edit(
                            self.scope.principal,
                            update.id,
                            ScheduleEdit {
                                request_id: stable_id(self.scope.session_id, invocation.as_ref()),
                                revision: update.revision,
                                title: edit.title,
                                prompt: edit.prompt,
                                timing: edit.timing,
                                notify: edit.notify,
                                enabled: edit.enabled,
                            },
                            now,
                        )
                        .await?;
                    return Ok(serde_json::to_string(&edited)?);
                }
                if update.edit.is_some() {
                    return Err("edit fields are only valid for action=edit".into());
                }
                json!(
                    store
                        .change(
                            self.scope.principal,
                            update.id,
                            update.revision,
                            &update.action,
                            now
                        )
                        .await?
                )
            }
            "schedule_runs" => {
                let run: Runs = serde_json::from_value(input)?;
                json!(store.runs(self.scope.principal, run.id).await?)
            }
            _ => {
                if input.as_object().is_none_or(|o| !o.is_empty()) {
                    return Err("schedule_list takes no arguments".into());
                }
                json!({"now_utc":chrono::Utc::now().to_rfc3339(),"schedules":store.list(self.scope.principal).await?})
            }
        };
        Ok(serde_json::to_string(&value)?)
    }
}
