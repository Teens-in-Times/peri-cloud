//! Account identity is captured by deployment, never supplied by the model.
use crate::state::CloudJournal;
use async_trait::async_trait;
use futures::StreamExt;
use peri_model::{
    Model, ModelCapabilities, ModelError, ModelRequest, ModelResult, ModelStream,
    PreparedModelRequest, ProtocolErrorKind,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone)]
pub(crate) struct PersonalScope {
    pub session_id: Uuid,
    pub turn_id: Uuid,
    pub device_id: Option<Uuid>,
    pub memory_fence: Arc<parking_lot::RwLock<Option<crate::memory::frames::Fence>>>,
    pub model_revision: String,
    pub private_memory: bool,
    pub scheduled: bool,
    pub schedule_push: bool,
    pub journal: Arc<CloudJournal>,
    pub principal: Uuid,
    pub max_bytes: usize,
}

pub(crate) struct MeteredModel {
    pub inner: Arc<dyn Model>,
    pub journal: Arc<CloudJournal>,
    pub principal: Uuid,
    pub daily_limit: u32,
    pub worker_budget: Option<(Uuid, u32, u32)>,
}
#[async_trait]
impl Model for MeteredModel {
    fn capabilities(&self) -> ModelCapabilities {
        self.inner.capabilities()
    }
    fn output_token_limit(&self) -> Option<u32> {
        self.inner.output_token_limit()
    }
    fn context_window(&self) -> Option<u32> {
        self.inner.context_window()
    }
    fn prepare_request(&self, request: &ModelRequest) -> ModelResult<PreparedModelRequest> {
        self.inner.prepare_request(request)
    }
    async fn stream(
        &self,
        mut request: ModelRequest,
        cancellation: CancellationToken,
    ) -> ModelResult<ModelStream> {
        if cancellation.is_cancelled() {
            return Err(ModelError::cancelled());
        }
        if let Some((_, capacity, reserve)) = self.worker_budget {
            // Keep the actual provider output limit within the reserved space,
            // even when the account model's default output is much larger.
            let output = reserve.min(capacity / 2).max(1);
            request.max_tokens = Some(
                request
                    .max_tokens
                    .unwrap_or(output)
                    .min(self.inner.output_token_limit().unwrap_or(output))
                    .min(output),
            );
        }
        let estimate = crate::tasks::windows::budget::estimate(&request);
        let prefix_estimate = crate::tasks::windows::budget::fixed_prefix(&request);
        if let Some((window, capacity, reserve)) = self.worker_budget {
            if !crate::tasks::windows::budget::preflight(
                &self.journal.identity_pool(),
                self.principal,
                window,
                estimate,
                capacity,
                reserve,
            )
            .await
            .map_err(|_| ModelError::cancelled())?
            {
                return Err(ModelError::protocol_with_summary(
                    ProtocolErrorKind::Other,
                    "worker_context_budget_boundary",
                ));
            }
        }
        let protocol = self
            .inner
            .prepare_request(&request)
            .ok()
            .map(|p| p.protocol().clone());
        let charged = self
            .journal
            .charge_model_request(self.principal, self.daily_limit)
            .await;
        if let Err(error) = charged {
            if !matches!(error, crate::state::StateError::Limit) {
                return Err(ModelError::protocol_with_summary(
                    ProtocolErrorKind::Other,
                    "account_model_accounting_unavailable",
                ));
            }
            if let Some((window, _, _)) = self.worker_budget {
                let tomorrow = (chrono::Utc::now().timestamp().div_euclid(86400) + 1) * 86400;
                sqlx::query("UPDATE agent_tasks SET record_json=json_set(record_json,'$.state','waiting_external','$.note','Account model quota unavailable; no new model request submitted.','$.wake_at',?,'$.revision',json_extract(record_json,'$.revision')+1) WHERE principal=? AND worker_session=? AND json_extract(record_json,'$.state')='running'")
                    .bind(tomorrow).bind(self.principal.to_string()).bind(window.to_string()).execute(&self.journal.identity_pool()).await.map_err(|_|ModelError::cancelled())?;
            }
            return Err(ModelError::protocol_with_summary(
                ProtocolErrorKind::Other,
                "account_model_request_limit",
            ));
        }
        let stream = self.inner.stream(request, cancellation).await?;
        let worker = self.worker_budget;
        let journal = self.journal.clone();
        let principal = self.principal;
        Ok(ModelStream::new(stream.then(move |event| {
            let journal = journal.clone();
            let protocol = protocol.clone();
            async move {
                if let (
                    Some((window, _, _)),
                    Ok(peri_model::ModelStreamEvent::Completed(response)),
                ) = (worker, &event)
                {
                    if crate::tasks::windows::budget::record(
                        &journal.identity_pool(),
                        principal,
                        window,
                        estimate,
                        prefix_estimate,
                        response,
                        protocol,
                    )
                    .await
                    .is_err()
                    {
                        return Err(ModelError::protocol_with_summary(
                            ProtocolErrorKind::Other,
                            "worker_budget_observation_not_saved",
                        ));
                    }
                }
                event
            }
        })))
    }
}
