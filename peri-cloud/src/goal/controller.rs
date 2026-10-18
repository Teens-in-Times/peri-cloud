//! Durable per-session goal state for the cloud turn loop.
//!
//! The controller is rebuilt at the start of every turn from `cloud_goals`.
//! Identity comes from the authenticated scope; the model can only name the
//! objective and the transitions, never the session or principal it writes to.

use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use chrono::Utc;
use parking_lot::RwLock;
use peri_acp_types::goal::{GoalController, GoalStatus, GoalViewSnapshot, ThreadGoal};
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::state::StateResult;

const MAX_OBJECTIVE_BYTES: usize = 8 * 1024;
const MAX_REASON_BYTES: usize = 2 * 1024;

/// The `WHERE` clause keeps a session's goal bound to its original principal:
/// a conflicting insert from another principal updates no row and fails.
const PERSIST: &str = "INSERT INTO cloud_goals(session_id,principal,goal_json,updated_at) \
     VALUES(?,?,?,?) \
     ON CONFLICT(session_id) DO UPDATE SET \
     goal_json=excluded.goal_json, updated_at=excluded.updated_at \
     WHERE cloud_goals.principal=excluded.principal";

pub(crate) struct CloudGoalController {
    pool: SqlitePool,
    session_id: Uuid,
    principal: Uuid,
    state: RwLock<Option<ThreadGoal>>,
    /// Serializes read-check-write transitions across concurrent tool calls.
    write_gate: Mutex<()>,
}

impl CloudGoalController {
    pub(crate) async fn load(
        pool: SqlitePool,
        session_id: Uuid,
        principal: Uuid,
    ) -> StateResult<Arc<dyn GoalController>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT goal_json FROM cloud_goals WHERE session_id=? AND principal=?")
                .bind(session_id.to_string())
                .bind(principal.to_string())
                .fetch_optional(&pool)
                .await?;
        let state = row.map(|(json,)| serde_json::from_str(&json)).transpose()?;
        Ok(Arc::new(Self {
            pool,
            session_id,
            principal,
            state: RwLock::new(state),
            write_gate: Mutex::new(()),
        }))
    }

    async fn persist(&self, goal: &ThreadGoal) -> Result<(), String> {
        let json = serde_json::to_string(goal).map_err(|error| {
            tracing::warn!(%error, session_id = %self.session_id, "goal serialization failed");
            "goal persistence failed".to_owned()
        })?;
        let result = sqlx::query(PERSIST)
            .bind(self.session_id.to_string())
            .bind(self.principal.to_string())
            .bind(json)
            .bind(Utc::now().timestamp())
            .execute(&self.pool)
            .await
            .map_err(|error| {
                tracing::warn!(%error, session_id = %self.session_id, "goal persistence failed");
                "goal persistence failed".to_owned()
            })?;
        if result.rows_affected() != 1 {
            return Err("goal persistence failed".to_owned());
        }
        Ok(())
    }
}

#[async_trait]
impl GoalController for CloudGoalController {
    async fn create_goal(&self, objective: String) -> Result<(), String> {
        let objective = objective.trim();
        if objective.is_empty() {
            return Err("goal objective cannot be empty".to_owned());
        }
        if objective.len() > MAX_OBJECTIVE_BYTES {
            return Err("goal objective is too long".to_owned());
        }
        let _gate = self.write_gate.lock().await;
        if self.state.read().is_some() {
            return Err("a goal already exists; clear it before creating another".to_owned());
        }
        let goal = ThreadGoal::new_at(objective.to_owned(), None, SystemTime::now());
        self.persist(&goal).await?;
        *self.state.write() = Some(goal);
        Ok(())
    }

    async fn complete_goal(&self) -> Result<(), String> {
        let _gate = self.write_gate.lock().await;
        let mut goal = self.active_goal("complete")?;
        goal.status = GoalStatus::Complete;
        goal.blocked_reason = None;
        goal.updated_at = Utc::now();
        self.persist(&goal).await?;
        *self.state.write() = Some(goal);
        Ok(())
    }

    async fn complete_goal_if_current(&self, expected_goal_id: &str) -> Result<(), String> {
        let _gate = self.write_gate.lock().await;
        let mut goal = self.active_goal("complete")?;
        if goal.goal_id != expected_goal_id {
            return Err("goal identity changed; completion was not applied".to_owned());
        }
        goal.status = GoalStatus::Complete;
        goal.blocked_reason = None;
        goal.updated_at = Utc::now();
        self.persist(&goal).await?;
        *self.state.write() = Some(goal);
        Ok(())
    }

    async fn block_goal(&self, reason: String) -> Result<(), String> {
        let reason = reason.trim();
        if reason.is_empty() {
            return Err("goal block reason cannot be empty".to_owned());
        }
        if reason.len() > MAX_REASON_BYTES {
            return Err("goal block reason is too long".to_owned());
        }
        let _gate = self.write_gate.lock().await;
        let mut goal = self.active_goal("block")?;
        goal.status = GoalStatus::Blocked;
        goal.blocked_reason = Some(reason.to_owned());
        goal.updated_at = Utc::now();
        self.persist(&goal).await?;
        *self.state.write() = Some(goal);
        Ok(())
    }

    async fn clear_goal(&self) -> Result<(), String> {
        let _gate = self.write_gate.lock().await;
        sqlx::query("DELETE FROM cloud_goals WHERE session_id=? AND principal=?")
            .bind(self.session_id.to_string())
            .bind(self.principal.to_string())
            .execute(&self.pool)
            .await
            .map_err(|error| {
                tracing::warn!(%error, session_id = %self.session_id, "goal deletion failed");
                "goal deletion failed".to_owned()
            })?;
        *self.state.write() = None;
        Ok(())
    }

    async fn increment_continuation(&self) -> Result<(), String> {
        let _gate = self.write_gate.lock().await;
        let Some(mut goal) = self.state.read().clone() else {
            return Ok(());
        };
        if goal.status != GoalStatus::Active {
            return Ok(());
        }
        goal.accounting.continuation_count = goal.accounting.continuation_count.saturating_add(1);
        goal.updated_at = Utc::now();
        self.persist(&goal).await?;
        *self.state.write() = Some(goal);
        Ok(())
    }

    fn snapshot(&self) -> GoalViewSnapshot {
        self.state
            .read()
            .as_ref()
            .map_or_else(GoalViewSnapshot::default, |goal| GoalViewSnapshot {
                goal_id: Some(goal.goal_id.clone()),
                objective: Some(goal.objective.clone()),
                status: Some(goal.status),
                token_budget: goal.token_budget,
                tokens_used: goal.accounting.tokens_used,
                time_used_seconds: goal.accounting.time_used_seconds,
                continuation_count: goal.accounting.continuation_count,
                blocked_reason: goal.blocked_reason.clone(),
                objective_just_updated: false,
            })
    }
}

impl CloudGoalController {
    fn active_goal(&self, action: &str) -> Result<ThreadGoal, String> {
        match self.state.read().as_ref() {
            None => Err(format!("no goal to {action}")),
            Some(goal) if goal.status != GoalStatus::Active => {
                Err(format!("goal is already {}", goal.status))
            }
            Some(goal) => Ok(goal.clone()),
        }
    }
}
