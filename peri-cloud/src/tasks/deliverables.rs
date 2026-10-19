//! Point-in-time deliverables and acceptance, backed by original executor receipts.
use super::*;
use async_trait::async_trait;
use peri_acp_types::tools::ToolExecutionStatus;
use peri_agent::{
    agent::react::{ToolCall, ToolResult},
    tools::{BaseTool, ToolContext},
};
use serde_json::{json, Value};
use sha2::Digest;
use sqlx::Row;

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Delivery {
    artifacts: Vec<Artifact>,
    verifications: Vec<Verification>,
    reviews: Vec<Review>,
}
impl Delivery {
    pub(super) fn summary(&self) -> Value {
        json!({"artifacts":self.artifacts.len(),"verifications":self.verifications.len(),"recorded_review":self.reviews.last().map(|r|json!({"outcome":r.outcome,"instruction_version":r.instruction_version,"unverified_preview":r.unverified.iter().take(4).map(|s|clip(s,200)).collect::<Vec<_>>(),"unverified_count":r.unverified.len()})),"effective_acceptance":"Inspect deliverables against current instructions and receipts."})
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct Artifact {
    id: Uuid,
    label: String,
    path: String,
    device: Uuid,
    workspace: String,
    operation: i64,
    receipt_version: String,
    content_version: Option<String>,
    instruction_version: i64,
}
#[derive(Clone, Serialize, Deserialize)]
struct Verification {
    id: Uuid,
    description: String,
    operation: i64,
    receipt_version: String,
    passed: bool,
    artifacts: Vec<Uuid>,
    instruction_version: i64,
}
#[derive(Clone, Serialize, Deserialize)]
struct Review {
    id: Uuid,
    outcome: String,
    reason: String,
    unverified: Vec<String>,
    verifications: Vec<Uuid>,
    instruction_version: i64,
    delivery_version: String,
    created_at: i64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    task_id: Uuid,
    expected_revision: Option<u64>,
    operation: Option<i64>,
    label: Option<String>,
    #[serde(default)]
    artifacts: Vec<Uuid>,
    #[serde(default)]
    verifications: Vec<Uuid>,
    outcome: Option<String>,
    reason: Option<String>,
    #[serde(default)]
    unverified: Vec<String>,
}
fn digest(value: &impl Serialize) -> Result<String> {
    Ok(format!(
        "sha256:{:x}",
        sha2::Sha256::digest(serde_json::to_vec(value)?)
    ))
}
fn delivery_version(delivery: &Delivery) -> Result<String> {
    digest(&(&delivery.artifacts, &delivery.verifications))
}
fn confirmed(result: &ToolResult) -> bool {
    !result.is_error
        && result.execution.as_ref().is_some_and(|e| {
            e.status == ToolExecutionStatus::Completed && e.exit_code.is_none_or(|code| code == 0)
        })
}
impl WorkerService {
    async fn delivery_operation(
        &self,
        record: &TaskRecord,
        id: i64,
    ) -> Result<(ToolCall, ToolResult, String, String, String)> {
        let row = sqlx::query("SELECT call_json,receipt_json,reconciled_json,native_tool,window_id,instruction_watermark FROM agent_task_operations WHERE task_id=? AND seq=?")
            .bind(record.id.to_string()).bind(id).fetch_optional(&self.store.pool).await?.ok_or(TaskError::Forbidden)?;
        let raw: String = row
            .get::<Option<String>, _>("reconciled_json")
            .or(row.get("receipt_json"))
            .ok_or(TaskError::Unavailable)?;
        let call: ToolCall = serde_json::from_str(row.get("call_json"))?;
        let result: ToolResult = serde_json::from_str(&raw)?;
        let native: String = row
            .get::<Option<String>, _>("native_tool")
            .ok_or(TaskError::Invalid)?;
        let version = digest(&(&call, &result))?;
        Ok((call, result, native, row.get("window_id"), version))
    }
    async fn delivery_current(&self, record: &TaskRecord, watermark: i64) -> Result<bool> {
        for artifact in &record.delivery.artifacts {
            let (_, result, _, _, version) =
                self.delivery_operation(record, artifact.operation).await?;
            if version != artifact.receipt_version
                || !confirmed(&result)
                || artifact.instruction_version != watermark
            {
                return Ok(false);
            }
            // A later mutation may replace or remove the file. Do not certify an old observation.
            let changed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_task_operations WHERE task_id=? AND seq>? AND native_tool IN ('Write','Edit','Bash')")
                .bind(record.id.to_string()).bind(artifact.operation).fetch_one(&self.store.pool).await?;
            if changed != 0 {
                return Ok(false);
            }
        }
        Ok(true)
    }
    async fn save_delivery(&self, record: &mut TaskRecord, watermark: i64) -> Result<()> {
        let revision = record.revision;
        record.revision = record.revision.saturating_add(1);
        let changed = sqlx::query("UPDATE agent_tasks SET record_json=? WHERE id=? AND principal=? AND json_extract(record_json,'$.revision')=? AND (SELECT COALESCE(MAX(seq),0) FROM agent_task_messages WHERE task_id=agent_tasks.id AND direction='parent')=?")
            .bind(serde_json::to_string(record)?).bind(record.id.to_string()).bind(record.principal.to_string()).bind(revision as i64).bind(watermark).execute(&self.store.pool).await?.rows_affected();
        if changed != 1 {
            return Err(TaskError::Conflict);
        }
        Ok(())
    }
    async fn delivery_request(
        &self,
        scope: &TaskScope,
        name: &str,
        args: Request,
    ) -> Result<Value> {
        let mut record = self.authorized(scope, args.task_id).await?;
        let watermark = self.instruction_watermark(record.id).await?;
        if name == "inspect_deliverables" {
            let version = delivery_version(&record.delivery)?;
            let current = self.delivery_current(&record, watermark).await?;
            let latest = record.delivery.reviews.last();
            let accepted = current
                && record.state == "completed"
                && latest.is_some_and(|r| {
                    r.outcome == "accepted"
                        && r.instruction_version == watermark
                        && r.delivery_version == version
                });
            return Ok(
                json!({"delivery":record.delivery,"delivery_version":version,"instruction_version":watermark,"evidence_current":current,"accepted":accepted,"point_in_time":true,"scope":"Task-local executor receipts; external later changes are not continuously monitored.","coverage_source":"agent_claim","revision":record.revision}),
            );
        }
        if args.expected_revision != Some(record.revision)
            || args.artifacts.len() > 16
            || args.verifications.len() > 32
            || args.unverified.len() > 16
            || args.unverified.iter().any(|s| s.len() > 1000)
        {
            return Err(TaskError::Conflict);
        }
        if name == "review_task" {
            if scope.role != AgentRole::Main || record.state != "completed" {
                return Err(TaskError::Invalid);
            }
            let outcome = args.outcome.ok_or(TaskError::Invalid)?;
            let reason = args
                .reason
                .filter(|s| !s.trim().is_empty() && s.len() <= 4000)
                .ok_or(TaskError::Invalid)?;
            if !matches!(outcome.as_str(), "accepted" | "rejected")
                || record.delivery.reviews.len() >= 32
            {
                return Err(TaskError::Invalid);
            }
            if outcome == "accepted" {
                if args.verifications.is_empty()
                    || !args.unverified.is_empty()
                    || !self.delivery_current(&record, watermark).await?
                {
                    return Err(TaskError::Conflict);
                }
                let mut covered = std::collections::HashSet::new();
                for id in &args.verifications {
                    let check = record
                        .delivery
                        .verifications
                        .iter()
                        .find(|v| v.id == *id)
                        .ok_or(TaskError::Invalid)?;
                    let (_, result, _, _, version) =
                        self.delivery_operation(&record, check.operation).await?;
                    if !check.passed
                        || !confirmed(&result)
                        || check.receipt_version != version
                        || check.instruction_version != watermark
                    {
                        return Err(TaskError::Conflict);
                    }
                    covered.extend(check.artifacts.iter().copied());
                }
                if record
                    .delivery
                    .artifacts
                    .iter()
                    .any(|a| !covered.contains(&a.id))
                {
                    return Err(TaskError::Conflict);
                }
                let unapplied: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_task_messages WHERE task_id=? AND direction='parent' AND state!='applied'").bind(record.id.to_string()).fetch_one(&self.store.pool).await?;
                if unapplied != 0 {
                    return Err(TaskError::Conflict);
                }
            }
            let review = Review {
                id: Uuid::new_v4(),
                outcome,
                reason,
                unverified: args.unverified,
                verifications: args.verifications,
                instruction_version: watermark,
                delivery_version: delivery_version(&record.delivery)?,
                created_at: now(),
            };
            record.delivery.reviews.push(review);
        } else {
            if scope.task != Some(record.id) || record.state != "running" {
                return Err(TaskError::Forbidden);
            }
            let operation = args.operation.ok_or(TaskError::Invalid)?;
            let observed: Option<i64> = sqlx::query_scalar(
                "SELECT instruction_watermark FROM agent_task_operations WHERE task_id=? AND seq=?",
            )
            .bind(record.id.to_string())
            .bind(operation)
            .fetch_optional(&self.store.pool)
            .await?;
            if observed != Some(watermark) {
                return Err(TaskError::Conflict);
            }
            let label = args
                .label
                .filter(|s| !s.trim().is_empty() && s.len() <= 1000)
                .ok_or(TaskError::Invalid)?;
            let (call, result, native, window, version) =
                self.delivery_operation(&record, operation).await?;
            if name == "record_artifact" {
                if !confirmed(&result)
                    || !matches!(native.as_str(), "Read" | "Write" | "Edit")
                    || record.delivery.artifacts.len() >= 16
                {
                    return Err(TaskError::Invalid);
                }
                let path = call.input["file_path"]
                    .as_str()
                    .ok_or(TaskError::Invalid)?
                    .to_string();
                let state = self
                    .journal
                    .session(
                        record.principal,
                        Uuid::parse_str(&window).map_err(|_| TaskError::Invalid)?,
                    )
                    .await
                    .map_err(|_| TaskError::Forbidden)?;
                let binding = state.frozen.binding.ok_or(TaskError::Invalid)?;
                let content_version = if native == "Write" {
                    call.input["content"]
                        .as_str()
                        .map(|s| format!("sha256:{:x}", sha2::Sha256::digest(s.as_bytes())))
                } else {
                    None
                };
                // Re-observing one path replaces its prior registration, not its original evidence.
                record.delivery.artifacts.retain(|a| {
                    a.path != path
                        || a.device != state.frozen.device_id.unwrap_or_default()
                        || a.workspace != binding.workspace
                });
                record.delivery.artifacts.push(Artifact {
                    id: Uuid::new_v4(),
                    label,
                    path,
                    device: state.frozen.device_id.ok_or(TaskError::Invalid)?,
                    workspace: binding.workspace,
                    operation,
                    receipt_version: version,
                    content_version,
                    instruction_version: watermark,
                });
            } else if name == "record_verification" {
                if record.delivery.verifications.len() >= 32
                    || !matches!(native.as_str(), "Read" | "Bash" | "Grep" | "Glob")
                {
                    return Err(TaskError::Invalid);
                }
                for id in &args.artifacts {
                    let artifact = record
                        .delivery
                        .artifacts
                        .iter()
                        .find(|a| a.id == *id)
                        .ok_or(TaskError::Invalid)?;
                    if operation < artifact.operation {
                        let changed:i64=sqlx::query_scalar("SELECT COUNT(*) FROM agent_task_operations WHERE task_id=? AND seq>? AND seq<=? AND native_tool IN ('Write','Edit','Bash')")
                            .bind(record.id.to_string()).bind(operation).bind(artifact.operation).fetch_one(&self.store.pool).await?;
                        if changed != 0 {
                            return Err(TaskError::Conflict);
                        }
                    }
                }
                record.delivery.verifications.push(Verification {
                    id: Uuid::new_v4(),
                    description: label,
                    operation,
                    receipt_version: version,
                    passed: confirmed(&result),
                    artifacts: args.artifacts,
                    instruction_version: watermark,
                });
            } else {
                return Err(TaskError::Invalid);
            }
        }
        self.save_delivery(&mut record, watermark).await?;
        let accepted = name == "review_task"
            && record
                .delivery
                .reviews
                .last()
                .is_some_and(|r| r.outcome == "accepted");
        Ok(
            json!({"delivery":record.delivery,"revision":record.revision,"instruction_version":watermark,"accepted":accepted,"evidence_is_point_in_time":true}),
        )
    }
}

pub(crate) async fn migrate(pool: &sqlx::SqlitePool) -> crate::state::StateResult<()> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    if version >= 21 {
        return Ok(());
    }
    if version != 20 {
        return Err(crate::state::StateError::Version);
    }
    let mut tx = pool.begin().await?;
    sqlx::query("ALTER TABLE agent_task_operations ADD COLUMN instruction_watermark INTEGER NOT NULL DEFAULT -1").execute(&mut *tx).await?;
    sqlx::query("PRAGMA user_version=21")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub(super) fn tools(scope: &TaskScope) -> Vec<Box<dyn BaseTool>> {
    let names: &[&str] = if scope.task.is_some() {
        &[
            "record_artifact",
            "record_verification",
            "inspect_deliverables",
        ]
    } else {
        &["review_task", "inspect_deliverables"]
    };
    names
        .iter()
        .map(|name| {
            Box::new(DeliveryTool {
                scope: scope.clone(),
                name,
            }) as Box<dyn BaseTool>
        })
        .collect()
}
struct DeliveryTool {
    scope: TaskScope,
    name: &'static str,
}
#[async_trait]
impl BaseTool for DeliveryTool {
    fn name(&self) -> &str {
        self.name
    }
    fn is_direct(&self) -> bool {
        true
    }
    fn description(&self) -> &str {
        match self.name {
            "record_artifact"=>"Register a point-in-time artifact from this task's successful native Read/Write/Edit operation seq. Host derives path, device, workspace and receipt version; do not invent a location or version. A later mutation invalidates the old observation; re-read after final changes. Registration is not acceptance.",
            "record_verification"=>"Record a native Read/Bash/Grep/Glob operation as validation evidence, linking artifact IDs. Host derives pass/failure from typed execution facts; description and semantic coverage are your claims. No mutation may intervene between validation and a later artifact observation. Use current requirements.",
            "review_task"=>"Main assistant reviews a completed execution against latest requirements. Accepted requires current artifact evidence, successful verification IDs covering all artifacts, applied parent instructions and no unverified items. Explain semantic coverage from actual evidence; model self-report alone is insufficient. Rejected records why; use message/control tools for follow-up work.",
            _=>"Inspect bounded task artifacts, validation evidence and acceptance history. accepted is computed against current instructions and receipt versions, separately from loop completion. Evidence is point-in-time, not continuous filesystem monitoring.",
        }
    }
    fn parameters(&self) -> Value {
        let mut props = json!({"task_id":{"type":"string","format":"uuid"}});
        let mut required = vec!["task_id"];
        if self.name != "inspect_deliverables" {
            props["expected_revision"] = json!({"type":"integer","minimum":0});
            required.push("expected_revision");
        }
        if matches!(self.name, "record_artifact" | "record_verification") {
            props["operation"] = json!({"type":"integer","minimum":1});
            props["label"] = json!({"type":"string","maxLength":1000});
            required.extend(["operation", "label"]);
        }
        if self.name == "record_verification" {
            props["artifacts"] =
                json!({"type":"array","maxItems":16,"items":{"type":"string","format":"uuid"}});
        }
        if self.name == "review_task" {
            props["outcome"] = json!({"type":"string","enum":["accepted","rejected"]});
            props["reason"] = json!({"type":"string","maxLength":4000});
            props["verifications"] =
                json!({"type":"array","maxItems":32,"items":{"type":"string","format":"uuid"}});
            props["unverified"] =
                json!({"type":"array","maxItems":16,"items":{"type":"string","maxLength":1000}});
            required.extend(["outcome", "reason"]);
        }
        json!({"type":"object","properties":props,"required":required,"additionalProperties":false})
    }
    async fn invoke(
        &self,
        input: Value,
        ctx: ToolContext<'_>,
    ) -> std::result::Result<String, Box<dyn std::error::Error + Send + Sync>> {
        if ctx.session_id.as_deref() != Some(self.scope.session.to_string().as_str()) {
            return Err(TaskError::Forbidden.into());
        }
        let service = self.scope.service.upgrade().ok_or(TaskError::Unavailable)?;
        Ok(serde_json::to_string(
            &service
                .delivery_request(&self.scope, self.name, serde_json::from_value(input)?)
                .await?,
        )?)
    }
}
