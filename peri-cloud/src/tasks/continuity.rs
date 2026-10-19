//! Durable execution boundaries and operation receipts, independent of trace previews.
use super::*;
use parking_lot::RwLock;
use peri_agent::agent::react::{ToolCall, ToolResult};
use peri_agent::session::MessageTranscript;
use serde_json::{json, Value};
use sha2::Digest;
use sqlx::{Row, SqlitePool};
use std::sync::Arc;

impl WorkerService {
    pub(super) async fn checkpoint(
        &self,
        scope: &TaskScope,
        turn: Uuid,
        transcript: &Arc<RwLock<MessageTranscript>>,
        phase: &str,
        calls: &[ToolCall],
        results: &[(ToolCall, ToolResult)],
    ) -> Result<()> {
        let Some(task) = scope.task else {
            return Ok(());
        };
        if !matches!(phase, "model_boundary" | "dispatching" | "tools_settled") {
            return Err(TaskError::Invalid);
        }
        let record = self.authorized(scope, task).await?;
        let active = self
            .journal
            .turn(scope.principal, scope.session, turn)
            .await
            .map_err(|_| TaskError::Forbidden)?;
        if active.state != crate::state::TurnState::Running
            || (active.cancel_requested && phase != "tools_settled")
            || record.worker_session != scope.session
            || record.turn.is_some_and(|id| id != turn)
        {
            return Err(TaskError::Conflict);
        }
        let mut snapshot = self
            .journal
            .session(scope.principal, scope.session)
            .await
            .map_err(|_| TaskError::Forbidden)?;
        {
            let current = transcript.read();
            snapshot.history = current.canonical_payloads();
            snapshot.history_flags = snapshot
                .history
                .iter()
                .map(|p| (p.id(), current.flags(p.id())))
                .collect();
        }
        // A transcript boundary does not prove the executor is idle. Recovery
        // must query the original authenticated executor before trusting this.
        snapshot.unsettled_jobs = None;
        let raw = serde_json::to_string(&snapshot)?;
        let digest = format!("{:x}", sha2::Sha256::digest(raw.as_bytes()));
        let mut tx = self.store.pool.begin().await?;
        for call in calls {
            let encoded = serde_json::to_string(call)?;
            let hash = format!("{:x}", sha2::Sha256::digest(encoded.as_bytes()));
            sqlx::query("INSERT OR IGNORE INTO agent_task_operations(task_id,turn_id,window_id,call_id,call_hash,call_json,receipt_json,intent_contract) VALUES(?,?,?,?,?,?,NULL,1)")
                .bind(task.to_string()).bind(turn.to_string()).bind(scope.session.to_string()).bind(&call.id).bind(&hash).bind(&encoded).execute(&mut *tx).await?;
            let previous: String = sqlx::query_scalar("SELECT call_hash FROM agent_task_operations WHERE task_id=? AND turn_id=? AND call_id=?")
                .bind(task.to_string()).bind(turn.to_string()).bind(&call.id).fetch_one(&mut *tx).await?;
            if previous != hash {
                return Err(TaskError::Conflict);
            }
        }
        for (call, result) in results {
            let native: Option<String> = sqlx::query_scalar("SELECT native_tool FROM agent_task_operations WHERE task_id=? AND turn_id=? AND call_id=?")
                .bind(task.to_string()).bind(turn.to_string()).bind(&call.id).fetch_optional(&mut *tx).await?.flatten();
            if call.id != result.tool_call_id
                || (call.name != result.tool_name
                    && native.as_deref() != Some(result.tool_name.as_str()))
            {
                return Err(TaskError::Conflict);
            }
            let encoded = serde_json::to_string(call)?;
            let hash = format!("{:x}", sha2::Sha256::digest(encoded.as_bytes()));
            // A policy-rejected call may have no dispatch intent. Preserve its
            // receipt too; it is evidence that no invocation was authorized.
            sqlx::query("INSERT OR IGNORE INTO agent_task_operations(task_id,turn_id,window_id,call_id,call_hash,call_json,receipt_json,intent_contract) VALUES(?,?,?,?,?,?,NULL,1)")
                .bind(task.to_string()).bind(turn.to_string()).bind(scope.session.to_string()).bind(&call.id).bind(&hash).bind(encoded).execute(&mut *tx).await?;
            let receipt = serde_json::to_string(result)?;
            let changed = sqlx::query("UPDATE agent_task_operations SET receipt_json=? WHERE task_id=? AND turn_id=? AND call_id=? AND call_hash=? AND (receipt_json IS NULL OR receipt_json=?)")
                .bind(&receipt).bind(task.to_string()).bind(turn.to_string()).bind(&call.id).bind(hash).bind(&receipt).execute(&mut *tx).await?.rows_affected();
            if changed != 1 {
                return Err(TaskError::Conflict);
            }
        }
        sqlx::query("INSERT INTO agent_task_checkpoints(task_id,turn_id,window_id,phase,snapshot_json,snapshot_hash,record_revision,created_at) VALUES(?,?,?,?,?,?,?,?) ON CONFLICT(task_id,turn_id) DO UPDATE SET phase=excluded.phase,snapshot_json=excluded.snapshot_json,snapshot_hash=excluded.snapshot_hash,record_revision=excluded.record_revision,created_at=excluded.created_at")
            .bind(task.to_string()).bind(turn.to_string()).bind(scope.session.to_string()).bind(phase).bind(raw).bind(digest).bind(record.revision as i64).bind(now()).execute(&mut *tx).await?;
        if phase == "model_boundary" {
            sqlx::query("UPDATE agent_task_checkpoints SET parent_watermark=(SELECT COALESCE(MAX(seq),0) FROM agent_task_messages WHERE task_id=? AND direction='parent' AND state IN ('consumed','applied')) WHERE task_id=? AND turn_id=?")
                .bind(task.to_string()).bind(task.to_string()).bind(turn.to_string()).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub(super) async fn operations(&self, task: Uuid, after: i64, limit: usize) -> Result<Value> {
        let rows = sqlx::query("SELECT seq,turn_id,window_id,call_id,call_hash,call_json,receipt_json,reconciled_json,invocation_id FROM agent_task_operations WHERE task_id=? AND seq>? ORDER BY seq LIMIT ?")
            .bind(task.to_string()).bind(after).bind(limit.clamp(1,16) as i64).fetch_all(&self.store.pool).await?;
        let mut entries = Vec::new();
        for row in rows {
            let call: ToolCall = serde_json::from_str(row.get("call_json"))?;
            let receipt: Option<String> = row
                .get::<Option<String>, _>("reconciled_json")
                .or(row.get("receipt_json"));
            let result: Option<ToolResult> =
                receipt.as_deref().map(serde_json::from_str).transpose()?;
            let input = call.input.to_string();
            entries.push(json!({"cursor":row.get::<i64,_>("seq"),"turn_id":row.get::<String,_>("turn_id"),"window_id":row.get::<String,_>("window_id"),"invocation_id":row.get::<Option<String>,_>("invocation_id"),"call_id":call.id,"name":call.name,"call_hash":row.get::<String,_>("call_hash"),"input_preview":clip(&input,800),"input_bytes":input.len(),"input_truncated":input.chars().count()>800,"receipt_recorded":result.is_some(),"execution":result.as_ref().and_then(|r|r.execution.as_ref()),"is_error":result.as_ref().map(|r|r.is_error),"output_preview":result.as_ref().map(|r|clip(&r.output,1200)),"output_truncated":result.as_ref().is_some_and(|r|r.output.chars().count()>1200)}));
        }
        Ok(
            json!({"next_cursor":entries.last().and_then(|e|e["cursor"].as_i64()).unwrap_or(after),"entries":entries,"previews":true,"missing_receipt":"unknown; never replay"}),
        )
    }
}

pub(crate) async fn migrate(pool: &SqlitePool) -> crate::state::StateResult<()> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    if version >= 18 {
        return Ok(());
    }
    if version != 17 {
        return Err(crate::state::StateError::Version);
    }
    let mut tx = pool.begin().await?;
    for sql in [
        "CREATE TABLE agent_task_checkpoints(task_id TEXT NOT NULL REFERENCES agent_tasks(id),turn_id TEXT NOT NULL REFERENCES cloud_turns(id),window_id TEXT NOT NULL REFERENCES cloud_sessions(id),phase TEXT NOT NULL,snapshot_json TEXT NOT NULL,snapshot_hash TEXT NOT NULL,record_revision INTEGER NOT NULL,created_at INTEGER NOT NULL,PRIMARY KEY(task_id,turn_id))",
        "CREATE TABLE agent_task_operations(seq INTEGER PRIMARY KEY AUTOINCREMENT,task_id TEXT NOT NULL REFERENCES agent_tasks(id),turn_id TEXT NOT NULL REFERENCES cloud_turns(id),window_id TEXT NOT NULL REFERENCES cloud_sessions(id),call_id TEXT NOT NULL,call_hash TEXT NOT NULL,call_json TEXT NOT NULL,receipt_json TEXT,UNIQUE(task_id,turn_id,call_id))",
        "PRAGMA user_version=18",
    ] { sqlx::query(sql).execute(&mut *tx).await?; }
    tx.commit().await?;
    Ok(())
}
