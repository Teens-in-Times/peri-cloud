//! Preparing an executor binding must not hold the main assistant's control gate.
use super::*;
use crate::state::FrozenSession;

impl WorkerService {
    pub(in crate::tasks) async fn reserve_worker_window(
        &self,
        record: &TaskRecord,
    ) -> Result<FrozenSession> {
        let mut rotation = self
            .rotation(record.id, record.worker_session)
            .await?
            .ok_or(TaskError::Conflict)?;
        if rotation.phase != RotationPhase::Prepared {
            return Err(TaskError::Conflict);
        }
        let previous = self
            .journal
            .session(record.principal, record.worker_session)
            .await
            .map_err(|_| TaskError::Unavailable)?;
        if let Some(id) = rotation.destination {
            return self
                .journal
                .session(record.principal, id)
                .await
                .map(|s| s.frozen)
                .map_err(|_| TaskError::Unavailable);
        }
        let gateway = self.gateway.upgrade().ok_or(TaskError::Unavailable)?;
        let frozen = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            gateway.fresh_worker_window(record),
        )
        .await
        .map_err(|_| TaskError::Unavailable)?
        .map_err(|_| TaskError::Unavailable)?;
        // An identical path that was replaced is not the original authorized workspace.
        if frozen.principal_id != record.principal
            || frozen.device_id != previous.frozen.device_id
            || frozen.workspace() != previous.frozen.workspace()
            || frozen.binding.as_ref().map(|b| &b.workspace_identity)
                != previous
                    .frozen
                    .binding
                    .as_ref()
                    .map(|b| &b.workspace_identity)
        {
            return Err(TaskError::Forbidden);
        }
        rotation.destination = Some(frozen.session_id);
        let changed=sqlx::query("UPDATE agent_window_rotations SET record_json=json_set(?,'$.cause',json_extract(record_json,'$.cause'),'$.prior_state',json_extract(record_json,'$.prior_state')) WHERE id=? AND phase='prepared' AND json_extract(record_json,'$.destination') IS NULL")
            .bind(serde_json::to_string(&rotation)?).bind(rotation.id.to_string()).execute(&self.store.pool).await?.rows_affected();
        if changed != 1 {
            return Err(TaskError::Conflict);
        }
        Ok(frozen)
    }
}
