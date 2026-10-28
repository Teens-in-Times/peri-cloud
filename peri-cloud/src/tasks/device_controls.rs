//! Controls resolve an original operation, never a newly prepared executor session.
use super::devices::{Prepare, PreparedTarget};
use super::*;
use peri_agent::tools::ToolOutput;
use serde_json::Value;
use sqlx::Row;
use std::sync::Arc;

impl WorkerService {
    pub(super) async fn control_target(
        &self,
        scope: &TaskScope,
        turn: Uuid,
        route: &Prepare,
        input: &Value,
    ) -> Result<Arc<PreparedTarget>> {
        let invocation = input["task_id"]
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or(TaskError::Invalid)?;
        let state = self
            .journal
            .session(scope.principal, scope.session)
            .await
            .map_err(|_| TaskError::Forbidden)?;
        let epoch = crate::memory::MemoryStore::new(&self.journal, scope.principal, 65536)
            .privacy_epoch()
            .await
            .map_err(|_| TaskError::Unavailable)?;
        let row = sqlx::query("SELECT id,frozen_json FROM cloud_native_operations WHERE principal=? AND source_session=? AND device_id=? AND invocation_id=? AND context_generation=? AND privacy_epoch=?")
            .bind(scope.principal.to_string()).bind(scope.session.to_string()).bind(route.device_id.to_string())
            .bind(invocation.to_string()).bind(state.context_generation as i64).bind(epoch as i64)
            .fetch_optional(&self.store.pool).await?.ok_or(TaskError::Forbidden)?;
        let frozen: crate::state::FrozenSession = serde_json::from_str(row.get("frozen_json"))?;
        if route
            .workspace
            .as_ref()
            .is_some_and(|w| w != &frozen.workspace())
        {
            return Err(TaskError::Forbidden);
        }
        let operation = Uuid::parse_str(row.get("id")).map_err(|_| TaskError::Invalid)?;
        let remote = self
            .gateway
            .upgrade()
            .ok_or(TaskError::Unavailable)?
            .reopen_native_target(scope.principal, &frozen)
            .await
            .map_err(|_| TaskError::Unavailable)?;
        self.register_target(
            scope,
            turn,
            route.workspace.clone(),
            frozen,
            remote,
            Some(operation),
        )
        .await
    }

    pub(super) async fn invoke_native_control(
        &self,
        target: &PreparedTarget,
        input: &Value,
        cancel: bool,
    ) -> Result<ToolOutput> {
        let operation = target
            .record
            .control_operation
            .ok_or(TaskError::Forbidden)?;
        let invocation = input["task_id"]
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or(TaskError::Invalid)?;
        let inserted = if cancel {
            sqlx::query("INSERT OR IGNORE INTO cloud_native_cancellations(operation_id,created_at,state) VALUES(?,?,'submitted_unknown')")
                .bind(operation.to_string()).bind(now()).execute(&self.store.pool).await?.rows_affected() == 1
        } else {
            false
        };
        // An ambiguous cancellation is observed, never automatically submitted again.
        let job = if inserted {
            target.remote.cancel_execution(invocation).await
        } else {
            target.remote.query_execution(invocation).await
        }
        .map_err(|_| TaskError::Unavailable)?;
        if job.invocation_id != invocation || job.session_id != target.remote.binding().session_id {
            return Err(TaskError::Forbidden);
        }
        if inserted {
            sqlx::query("UPDATE cloud_native_cancellations SET state='response_received',receipt_json=? WHERE operation_id=?")
                .bind(serde_json::to_string(&job)?).bind(operation.to_string()).execute(&self.store.pool).await?;
        }
        let _ = self.query_native(operation).await;
        Ok(peri_remote_tools::snapshot_output(job))
    }
}
