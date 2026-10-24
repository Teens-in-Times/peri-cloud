//! Cooperative stops occur at a persisted boundary, never by abandoning a tool future.
use super::*;
use peri_agent::error::AgentError;
use serde_json::json;

impl WorkerService {
    pub(super) async fn flush_worker_messages(&self, record: &TaskRecord) -> Result<()> {
        if record.terminal() || record.state == "cancel_requested" {
            return Ok(());
        }
        let gateway = self.gateway.upgrade().ok_or(TaskError::Unavailable)?;
        let pending: Vec<(i64,String)> = sqlx::query_as("SELECT seq,body FROM agent_task_messages WHERE task_id=? AND direction='worker' AND state='submitted' ORDER BY seq LIMIT 8")
            .bind(record.id.to_string()).fetch_all(&self.store.pool).await?;
        for (id, body) in pending {
            if gateway
                .notify_worker_message(record, id, &body)
                .await
                .is_ok()
            {
                sqlx::query("UPDATE agent_task_messages SET state='forwarded' WHERE task_id=? AND seq=? AND direction='worker' AND state='submitted'")
                    .bind(record.id.to_string()).bind(id).execute(&self.store.pool).await?;
            }
        }
        Ok(())
    }
    pub(super) async fn request_control(
        &self,
        scope: &TaskScope,
        id: Uuid,
        revision: u64,
        action: &str,
    ) -> Result<TaskRecord> {
        if action == "cancel" {
            return self.request_cancel(scope, id, revision).await;
        }
        let gateway = self.gateway.upgrade().ok_or(TaskError::Unavailable)?;
        let gate = gateway.worker_control_gate(scope.principal);
        let _guard = gate.lock().await;
        let mut record = self.authorized(scope, id).await?;
        if record.revision != revision {
            return Err(TaskError::Conflict);
        }
        if action == "rotate" && record.state == "window_ready" {
            return Ok(record);
        }
        if action == "resume" && record.state == "rotate_requested" {
            return Err(TaskError::Conflict);
        }
        if action == "resume" {
            record.manual_goal_guard = false;
        }
        if action == "rotate" {
            record.manual_goal_guard = true;
        }
        if record.terminal()
            && !(action == "resume" && matches!(record.state.as_str(), "failed" | "completed"))
        {
            return Err(TaskError::Invalid);
        }
        if record.turn.is_none() {
            record.turn = gateway
                .worker_admission(&record)
                .await
                .map_err(|_| TaskError::Unavailable)?
                .map(|t| t.turn_id);
        }
        if action == "rotate" && record.turn.is_none() {
            return Err(TaskError::Invalid);
        }
        let running = if let Some(turn) = record.turn {
            matches!(
                self.journal
                    .turn(record.principal, record.worker_session, turn)
                    .await
                    .map_err(|_| TaskError::Unavailable)?
                    .state,
                crate::state::TurnState::Queued | crate::state::TurnState::Running
            )
        } else {
            false
        };
        if action == "rotate" {
            self.begin_rotation(&mut record, super::windows::RotationCause::Manual)
                .await?;
            return Ok(record);
        }
        record.state = match action {
            "pause" => {
                if running {
                    "pause_requested"
                } else {
                    "paused"
                }
            }
            "resume" if record.turn.is_none() => "queued",
            "resume" if running => "running",
            "resume" => "resume_requested",
            "rotate" => "rotate_requested",
            _ => return Err(TaskError::Invalid),
        }
        .into();
        if action != "rotate" {
            record.wake_at = None;
        }
        record.notified = false;
        self.store.save(&mut record).await?;
        Ok(record)
    }

    pub(super) async fn wait_external(
        &self,
        scope: &TaskScope,
        reason: &str,
        seconds: Option<u64>,
    ) -> Result<serde_json::Value> {
        let id = scope.task.ok_or(TaskError::Forbidden)?;
        if reason.trim().is_empty() || reason.len() > 4000 || seconds.is_some_and(|s| s > 604800) {
            return Err(TaskError::Invalid);
        }
        let mut record = self.authorized(scope, id).await?;
        if record.state != "running" {
            return Err(TaskError::Conflict);
        }
        record.state = "waiting_external".into();
        record.note = reason.into();
        record.wake_at = seconds.map(|s| now().saturating_add(s as i64));
        self.store.save(&mut record).await?;
        Ok(
            json!({"state":record.state,"wake_at":record.wake_at,"instruction":"No more task work now. The host stops at the next safe boundary and frees the loop slot. Main assistant may resume explicitly."}),
        )
    }

    pub(super) async fn parent_replied(&self, record: &TaskRecord) -> Result<bool> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_task_messages WHERE task_id=? AND direction='parent' AND seq>?")
            .bind(record.id.to_string()).bind(record.wait_after).fetch_one(&self.store.pool).await?;
        Ok(count > 0)
    }

    pub(super) async fn boundary_stop(
        &self,
        scope: &TaskScope,
        estimated: usize,
        progressed: bool,
        context_window: u32,
        step: usize,
        max_iterations: usize,
    ) -> std::result::Result<(), AgentError> {
        let Some(task) = scope.task else {
            return Ok(());
        };
        let mut record = self
            .authorized(scope, task)
            .await
            .map_err(|_| AgentError::Interrupted)?;
        if matches!(record.state.as_str(), "queued" | "waiting_device") {
            record.state = "running".into();
            self.store
                .save(&mut record)
                .await
                .map_err(|_| AgentError::Interrupted)?;
        }
        if record.manual_goal_guard && record.state == "running" {
            let active = super::windows::active_parent_goal(
                &self.store.pool,
                record.principal,
                record.parent_session,
            )
            .await
            .map_err(|_| AgentError::Interrupted)?;
            if record.parent_goal.is_none() || record.parent_goal != active {
                record.state = "window_ready".into();
            } else {
                record.manual_goal_guard = false;
            }
            self.store
                .save(&mut record)
                .await
                .map_err(|_| AgentError::Interrupted)?;
        }
        if record.state == "waiting_parent" && self.parent_replied(&record).await.unwrap_or(false) {
            record.state = "running".into();
            self.store
                .save(&mut record)
                .await
                .map_err(|_| AgentError::Interrupted)?;
        }
        let budget = self
            .gateway
            .upgrade()
            .map(|g| {
                g.limits()
                    .worker_window_tokens
                    .min(context_window)
                    .saturating_sub(g.limits().worker_reserve_tokens.min(context_window / 2))
                    as usize
            })
            .unwrap_or(40000);
        if progressed && estimated >= budget && record.state == "running" {
            let observed = super::windows::budget::observation(
                &self.store.pool,
                scope.principal,
                scope.session,
            )
            .await
            .map_err(|_| AgentError::Interrupted)?;
            if observed.requests == 0 && record.run_number > 0 {
                record.state = "waiting_external".into();
                record.note="Fresh window exceeds context budget before any model work; waiting for a smaller contract or larger capacity.".into();
                record.wake_at = None;
                self.store
                    .save(&mut record)
                    .await
                    .map_err(|_| AgentError::Interrupted)?;
                return Err(AgentError::Interrupted);
            }
            self.begin_rotation(&mut record, super::windows::RotationCause::Budget)
                .await
                .map_err(|_| AgentError::Interrupted)?;
        }
        if step >= max_iterations && record.state == "running" {
            record.state = "resume_requested".into();
            self.store
                .save(&mut record)
                .await
                .map_err(|_| AgentError::Interrupted)?;
        }
        if matches!(
            record.state.as_str(),
            "pause_requested"
                | "paused"
                | "waiting_parent"
                | "waiting_external"
                | "rotate_requested"
                | "resume_requested"
                | "cancel_requested"
                | "invalidated"
                | "window_ready"
        ) {
            return Err(AgentError::Interrupted);
        }
        Ok(())
    }
}
