//! Typed approval events handled by the existing main conversation and permission broker.
use super::*;
use serde_json::{json, Value};
use sqlx::{Row, SqlitePool};

pub(crate) fn is_control_turn(turn: &crate::state::TurnRecord) -> bool {
    turn.request_key.starts_with("task-event:")
        && matches!(&turn.prompt, peri_acp_types::messages::MessageContent::Text(text)
            if serde_json::from_str::<Value>(text).is_ok_and(|v| v["source"] == "host_approval_event"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SubmitDecision {
    pub request_id: Uuid,
    pub approve: bool,
    #[serde(default)]
    pub deletion_possible: bool,
    pub reason: String,
}

impl WorkerService {
    /// Only complete, current requests get a presentation receipt for this main turn.
    pub(super) async fn approval_frames(
        &self,
        scope: &TaskScope,
        turn: Uuid,
        budget: usize,
    ) -> Result<Vec<Value>> {
        if scope.role != AgentRole::Main || scope.task.is_some() {
            return Err(TaskError::Forbidden);
        }
        let rows = sqlx::query("SELECT id,task_id,window_id,turn_id,request_hash,instruction_watermark,request_json,expires_at FROM agent_task_approvals WHERE principal=? AND parent_session=? AND state='reviewing' AND expires_at>? ORDER BY rowid LIMIT 4")
            .bind(scope.principal.to_string()).bind(scope.session.to_string()).bind(now()).fetch_all(&self.store.pool).await?;
        let mut frames = Vec::new();
        let mut used = 0;
        for row in rows {
            let task = Uuid::parse_str(row.get("task_id")).map_err(|_| TaskError::Invalid)?;
            let record = match self.authorized(scope, task).await {
                Ok(record) => record,
                Err(TaskError::Forbidden | TaskError::Conflict) => continue,
                Err(error) => return Err(error),
            };
            let worker_scope = self.scope_record(&record);
            let worker_turn =
                Uuid::parse_str(row.get("turn_id")).map_err(|_| TaskError::Invalid)?;
            let watermark: i64 = row.get("instruction_watermark");
            if record.worker_session.to_string() != row.get::<String, _>("window_id")
                || self
                    .review_current(&worker_scope, worker_turn, watermark)
                    .await
                    .is_err()
            {
                continue;
            }
            let calls: Vec<peri_acp_types::interaction::ApprovalItem> =
                serde_json::from_str(row.get("request_json"))?;
            let worker = self
                .journal
                .session(scope.principal, record.worker_session)
                .await
                .map_err(|_| TaskError::Forbidden)?;
            let target = json!({"device_id":worker.frozen.device_id,"workspace":worker.frozen.workspace(),"execution_session":record.execution_session,"window_id":record.worker_session,"turn_id":worker_turn});
            let risks: Vec<_> = calls.iter().map(|call| json!({"call_id":call.tool_call_id,"deletion_possible":super::deletion::possible(&call.tool_name, &call.tool_input),"analysis":"native tool semantics and bounded literal shell analysis; opaque code remains possible"})).collect();
            let frame = json!({"request_id":row.get::<String,_>("id"),"task_id":task,"target":target,"instruction_version":watermark,"goal":record.goal,"constraints":record.constraints,"acceptance":record.acceptance,"parent_messages":self.store.parent_context(task).await?,"exact_calls":calls,"host_risk_analysis":risks,"complete":true});
            let size = serde_json::to_vec(&frame)?.len();
            if used + size > budget {
                continue;
            }
            used += size;
            sqlx::query("INSERT OR IGNORE INTO agent_task_approval_presentations(request_id,main_turn,request_hash,expires_at) VALUES(?,?,?,?)")
                .bind(row.get::<String,_>("id")).bind(turn.to_string()).bind(row.get::<String,_>("request_hash")).bind(row.get::<i64,_>("expires_at")).execute(&self.store.pool).await?;
            frames.push(frame);
        }
        sqlx::query("DELETE FROM agent_task_approval_presentations WHERE expires_at<=?")
            .bind(now())
            .execute(&self.store.pool)
            .await?;
        Ok(frames)
    }

    pub(super) async fn decide_approval(
        &self,
        scope: &TaskScope,
        turn: Uuid,
        input: SubmitDecision,
    ) -> Result<Value> {
        if scope.role != AgentRole::Main
            || scope.task.is_some()
            || input.reason.trim().is_empty()
            || input.reason.len() > 2000
        {
            return Err(TaskError::Forbidden);
        }
        let gateway = self.gateway.upgrade().ok_or(TaskError::Unavailable)?;
        let gate = gateway.worker_control_gate(scope.principal);
        let _guard = gate.lock().await;
        let active = self
            .journal
            .turn(scope.principal, scope.session, turn)
            .await
            .map_err(|_| TaskError::Forbidden)?;
        if active.state != crate::state::TurnState::Running || active.cancel_requested {
            return Err(TaskError::Conflict);
        }
        let row = sqlx::query("SELECT a.task_id,a.turn_id,a.window_id,a.instruction_watermark FROM agent_task_approvals a JOIN agent_task_approval_presentations p ON p.request_id=a.id AND p.request_hash=a.request_hash WHERE a.id=? AND a.principal=? AND a.parent_session=? AND p.main_turn=? AND a.state='reviewing' AND a.expires_at>?")
            .bind(input.request_id.to_string()).bind(scope.principal.to_string()).bind(scope.session.to_string()).bind(turn.to_string()).bind(now()).fetch_optional(&self.store.pool).await?.ok_or(TaskError::Forbidden)?;
        let task = Uuid::parse_str(row.get("task_id")).map_err(|_| TaskError::Invalid)?;
        let record = self.authorized(scope, task).await?;
        if record.worker_session.to_string() != row.get::<String, _>("window_id") {
            return Err(TaskError::Conflict);
        }
        self.review_current(
            &self.scope_record(&record),
            Uuid::parse_str(row.get("turn_id")).map_err(|_| TaskError::Invalid)?,
            row.get("instruction_watermark"),
        )
        .await?;
        let decision = super::approvals::ReviewDecision {
            approve: input.approve,
            deletion_possible: input.deletion_possible,
            reason: input.reason,
            notice: String::new(),
            source: super::approvals::ReviewSource::MainAgent,
        };
        let changed = sqlx::query("UPDATE agent_task_approvals SET state='decided',decision_json=? WHERE id=? AND state='reviewing' AND expires_at>?")
            .bind(serde_json::to_string(&decision)?).bind(input.request_id.to_string()).bind(now()).execute(&self.store.pool).await?.rows_affected();
        if changed != 1 {
            return Err(TaskError::Conflict);
        }
        Ok(
            json!({"request_id":input.request_id,"decision_recorded":true,"execution_authorized":false,"next":"permission middleware and deletion notification settlement"}),
        )
    }
}

pub(crate) async fn migrate(pool: &SqlitePool) -> crate::state::StateResult<()> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    if version >= 24 {
        return Ok(());
    }
    if version != 23 {
        return Err(crate::state::StateError::Version);
    }
    let mut tx = pool.begin().await?;
    sqlx::query("CREATE TABLE agent_task_approval_presentations(request_id TEXT NOT NULL REFERENCES agent_task_approvals(id),main_turn TEXT NOT NULL REFERENCES cloud_turns(id),request_hash TEXT NOT NULL,expires_at INTEGER NOT NULL,PRIMARY KEY(request_id,main_turn))").execute(&mut *tx).await?;
    sqlx::query("PRAGMA user_version=24")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
