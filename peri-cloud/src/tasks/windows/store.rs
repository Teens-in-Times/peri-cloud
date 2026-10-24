use super::*;
use peri_acp_types::goal::{GoalStatus, ThreadGoal};
use sqlx::SqlitePool;

pub(in crate::tasks) async fn active_parent_goal_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    principal: Uuid,
    session: Uuid,
) -> Result<Option<String>> {
    let raw: Option<String> =
        sqlx::query_scalar("SELECT goal_json FROM cloud_goals WHERE principal=? AND session_id=?")
            .bind(principal.to_string())
            .bind(session.to_string())
            .fetch_optional(&mut **tx)
            .await?;
    let goal: Option<ThreadGoal> = raw.as_deref().map(serde_json::from_str).transpose()?;
    Ok(goal
        .filter(|g| {
            g.status == GoalStatus::Active
                && g.token_budget.is_none_or(|b| g.accounting.tokens_used < b)
        })
        .map(|g| g.goal_id))
}

pub(in crate::tasks) async fn active_parent_goal(
    pool: &SqlitePool,
    principal: Uuid,
    session: Uuid,
) -> Result<Option<String>> {
    let raw: Option<String> =
        sqlx::query_scalar("SELECT goal_json FROM cloud_goals WHERE principal=? AND session_id=?")
            .bind(principal.to_string())
            .bind(session.to_string())
            .fetch_optional(pool)
            .await?;
    let goal: Option<ThreadGoal> = raw.as_deref().map(serde_json::from_str).transpose()?;
    Ok(goal
        .filter(|g| {
            g.status == GoalStatus::Active
                && g.token_budget.is_none_or(|b| g.accounting.tokens_used < b)
        })
        .map(|g| g.goal_id))
}

impl WorkerService {
    /// Persist control intent and task state together; failed CAS leaves no accepted rotation.
    pub(in crate::tasks) async fn begin_rotation(
        &self,
        record: &mut TaskRecord,
        cause: RotationCause,
    ) -> Result<()> {
        let revision = record.revision;
        let turn = record.turn.ok_or(TaskError::Invalid)?;
        let mut tx = self.store.pool.begin().await?;
        let actual:Option<String>=sqlx::query_scalar("SELECT record_json FROM agent_tasks WHERE id=? AND principal=? AND json_extract(record_json,'$.revision')=?")
            .bind(record.id.to_string()).bind(record.principal.to_string()).bind(revision as i64).fetch_optional(&mut *tx).await?;
        let actual: TaskRecord = serde_json::from_str(&actual.ok_or(TaskError::Conflict)?)?;
        if actual.worker_session != record.worker_session || actual.terminal() {
            return Err(TaskError::Conflict);
        }
        let raw:Option<String>=sqlx::query_scalar("SELECT record_json FROM agent_window_rotations WHERE task_id=? AND source_window=? AND phase IN ('requested','summarizing','prepared') ORDER BY rowid DESC LIMIT 1")
            .bind(record.id.to_string()).bind(record.worker_session.to_string()).fetch_optional(&mut *tx).await?;
        let mut previous: Option<Rotation> =
            raw.as_deref().map(serde_json::from_str).transpose()?;
        if previous.as_ref().is_some_and(|r| r.source_turn != turn) {
            let old = previous.take().ok_or(TaskError::Conflict)?;
            if old.phase == RotationPhase::Summarizing {
                return Err(TaskError::Conflict);
            }
            sqlx::query("UPDATE agent_window_rotations SET phase='aborted',record_json=json_set(record_json,'$.phase','aborted') WHERE id=?")
                .bind(old.id.to_string()).execute(&mut *tx).await?;
        }
        let watermark:i64=sqlx::query_scalar("SELECT COALESCE(MAX(seq),0) FROM agent_task_messages WHERE task_id=? AND direction='parent'")
            .bind(record.id.to_string()).fetch_one(&mut *tx).await?;
        let mut rotation = previous.unwrap_or_else(|| Rotation {
            id: Uuid::new_v4(),
            task: record.id,
            source_window: record.worker_session,
            source_turn: turn,
            checkpoint_hash: String::new(),
            instruction_watermark: watermark,
            cause,
            prior_state: actual.state.clone(),
            phase: RotationPhase::Requested,
            summary: None,
            summary_status: "not_started".into(),
            usage: None,
            destination: None,
            created_at: now(),
        });
        if cause == RotationCause::Manual {
            rotation.cause = cause;
        }
        if actual.state != "rotate_requested" {
            rotation.prior_state = actual.state;
        }
        let phase = serde_json::to_value(rotation.phase)?;
        sqlx::query("INSERT INTO agent_window_rotations(id,task_id,source_window,phase,record_json) VALUES(?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET record_json=excluded.record_json")
            .bind(rotation.id.to_string()).bind(record.id.to_string()).bind(record.worker_session.to_string())
            .bind(phase.as_str().ok_or(TaskError::Invalid)?).bind(serde_json::to_string(&rotation)?).execute(&mut *tx).await?;
        record.state = "rotate_requested".into();
        record.notified = false;
        record.revision += 1;
        if cause == RotationCause::Manual {
            record.manual_goal_guard = true;
        }
        let n=sqlx::query("UPDATE agent_tasks SET record_json=? WHERE id=? AND principal=? AND json_extract(record_json,'$.revision')=?")
            .bind(serde_json::to_string(record)?).bind(record.id.to_string()).bind(record.principal.to_string()).bind(revision as i64).execute(&mut *tx).await?.rows_affected();
        if n != 1 {
            return Err(TaskError::Conflict);
        }
        tx.commit().await?;
        Ok(())
    }
    pub(in crate::tasks) async fn rotation(
        &self,
        task: Uuid,
        window: Uuid,
    ) -> Result<Option<Rotation>> {
        let raw: Option<String> = sqlx::query_scalar(
            "SELECT record_json FROM agent_window_rotations WHERE task_id=? AND source_window=? ORDER BY rowid DESC LIMIT 1",
        )
        .bind(task.to_string())
        .bind(window.to_string())
        .fetch_optional(&self.store.pool)
        .await?;
        raw.as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(Into::into)
    }

    pub(in crate::tasks) async fn request_rotation(
        &self,
        record: &TaskRecord,
        cause: RotationCause,
    ) -> Result<Rotation> {
        if let Some(mut previous) = self.rotation(record.id, record.worker_session).await? {
            if previous.phase != RotationPhase::Aborted && record.turn == Some(previous.source_turn)
            {
                return Ok(previous);
            }
            if previous.phase == RotationPhase::Summarizing {
                return Err(TaskError::Conflict);
            }
            if previous.phase != RotationPhase::Aborted {
                let expected = previous.phase;
                previous.phase = RotationPhase::Aborted;
                self.save_rotation(&previous, expected).await?;
            }
        }
        let turn = record.turn.ok_or(TaskError::Invalid)?;
        let watermark: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq),0) FROM agent_task_messages WHERE task_id=? AND direction='parent'",
        ).bind(record.id.to_string()).fetch_one(&self.store.pool).await?;
        let rotation = Rotation {
            id: Uuid::new_v4(),
            task: record.id,
            source_window: record.worker_session,
            source_turn: turn,
            checkpoint_hash: String::new(),
            instruction_watermark: watermark,
            cause,
            prior_state: record.state.clone(),
            phase: RotationPhase::Requested,
            summary: None,
            summary_status: "not_started".into(),
            usage: None,
            destination: None,
            created_at: now(),
        };
        sqlx::query("INSERT OR IGNORE INTO agent_window_rotations(id,task_id,source_window,phase,record_json) VALUES(?,?,?,'requested',?)")
            .bind(rotation.id.to_string()).bind(record.id.to_string()).bind(record.worker_session.to_string())
            .bind(serde_json::to_string(&rotation)?).execute(&self.store.pool).await?;
        self.rotation(record.id, record.worker_session)
            .await?
            .ok_or(TaskError::Conflict)
    }

    pub(in crate::tasks) async fn save_rotation(
        &self,
        value: &Rotation,
        expected: RotationPhase,
    ) -> Result<()> {
        let phase = serde_json::to_value(value.phase)?;
        let expected = serde_json::to_value(expected)?;
        let n = sqlx::query(
            "UPDATE agent_window_rotations SET phase=?,record_json=json_set(?,'$.cause',json_extract(record_json,'$.cause'),'$.prior_state',json_extract(record_json,'$.prior_state')) WHERE id=? AND phase=?",
        )
        .bind(phase.as_str().ok_or(TaskError::Invalid)?)
        .bind(serde_json::to_string(value)?)
        .bind(value.id.to_string())
        .bind(expected.as_str().ok_or(TaskError::Invalid)?)
        .execute(&self.store.pool)
        .await?
        .rows_affected();
        if n != 1 {
            return Err(TaskError::Conflict);
        }
        Ok(())
    }
}

pub(crate) async fn migrate(pool: &SqlitePool) -> crate::state::StateResult<()> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    if version < 26 {
        if version != 25 {
            return Err(crate::state::StateError::Version);
        }
        let mut tx = pool.begin().await?;
        sqlx::query("CREATE TABLE agent_window_rotations(id TEXT PRIMARY KEY,task_id TEXT NOT NULL REFERENCES agent_tasks(id),source_window TEXT NOT NULL REFERENCES cloud_sessions(id),phase TEXT NOT NULL,record_json TEXT NOT NULL)")
            .execute(&mut *tx).await?;
        sqlx::query("CREATE UNIQUE INDEX one_pending_window_rotation ON agent_window_rotations(task_id,source_window) WHERE phase IN ('requested','summarizing','prepared')")
            .execute(&mut *tx).await?;
        sqlx::query("CREATE TABLE agent_window_budget(window_id TEXT PRIMARY KEY REFERENCES cloud_sessions(id),principal TEXT NOT NULL,record_json TEXT NOT NULL)")
            .execute(&mut *tx).await?;
        sqlx::query("PRAGMA user_version=26")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    // Only called during journal open, before any owned model request can run.
    // A submitted summary with no durable result is unknown, never re-submitted.
    sqlx::query("UPDATE agent_window_rotations SET phase='prepared',record_json=json_set(record_json,'$.phase','prepared','$.summary_status','unknown_after_restart') WHERE phase='summarizing'")
        .execute(pool).await?;
    Ok(())
}
