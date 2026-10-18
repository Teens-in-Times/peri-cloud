//! Account identity is captured by deployment, never supplied by the model.
use crate::state::CloudJournal;
use async_trait::async_trait;
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
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> ModelResult<ModelStream> {
        if cancellation.is_cancelled() {
            return Err(ModelError::cancelled());
        }
        self.journal
            .charge_model_request(self.principal, self.daily_limit)
            .await
            .map_err(|_| {
                ModelError::protocol_with_summary(
                    ProtocolErrorKind::Other,
                    "account_model_request_limit",
                )
            })?;
        self.inner.stream(request, cancellation).await
    }
}
