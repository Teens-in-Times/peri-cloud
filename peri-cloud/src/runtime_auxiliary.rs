//! Auxiliary model work shares background capacity, ownership and shutdown cancellation.
use super::*;
use std::future::Future;

impl CloudRuntime {
    pub(crate) fn cancel_auxiliary(&self, principal: Uuid, session: Uuid) {
        for task in self.ownership.lock().tasks.values() {
            if task.principal_id == principal
                && task.session_id == session
                && task.turn_id.is_none()
            {
                task.cancellation.cancel();
            }
        }
    }
    pub(crate) fn try_spawn_auxiliary<F, Fut>(
        &self,
        id: Uuid,
        principal: Uuid,
        session: Uuid,
        work: F,
    ) -> Result<bool, RuntimeError>
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), RuntimeError>> + Send + 'static,
    {
        let mut owner = self.ownership.lock();
        if owner.closing {
            return Err(RuntimeError::Closing);
        }
        let active: Vec<_> = owner
            .tasks
            .values()
            .filter(|t| !t.handle.is_finished() && !t.parked)
            .collect();
        if active.iter().any(|t| t.session_id == session)
            || active.iter().filter(|t| t.background).count() >= self.limits.max_background_turns
            || active
                .iter()
                .filter(|t| t.background && t.principal_id == principal)
                .count()
                >= self.limits.max_background_per_user
        {
            return Ok(false);
        }
        if owner.tasks.contains_key(&id) {
            return Ok(false);
        }
        let cancellation = CancellationToken::new();
        let handle = tokio::spawn(work(cancellation.clone()));
        owner.tasks.insert(
            id,
            OwnedTask {
                background: true,
                parked: false,
                principal_id: principal,
                session_id: session,
                turn_id: None,
                cancellation,
                handle,
            },
        );
        Ok(true)
    }
}
