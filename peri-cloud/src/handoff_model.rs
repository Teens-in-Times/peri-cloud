//! Reuse the original account model route, without installing tools or a worker loop.
use super::*;

impl CloudAgent {
    pub(crate) async fn handoff_summary(
        &self,
        journal: Arc<crate::state::CloudJournal>,
        principal: Uuid,
        source_turn: Uuid,
        limit: u32,
        mut request: peri_model::ModelRequest,
        cancellation: CancellationToken,
    ) -> peri_model::ModelResult<peri_model::ModelResponse> {
        let inner = match &self.account_models {
            Some(models) => models
                .resolve_for_turn(principal, self.session_id, source_turn, self.model.clone())
                .await
                .map_err(|_| peri_model::ModelError::cancelled())?,
            None => self.model.clone(),
        };
        if let Some(limit) = inner.output_token_limit() {
            request.max_tokens = Some(request.max_tokens.unwrap_or(limit).min(limit));
        }
        let model = crate::personal::MeteredModel {
            inner,
            journal,
            principal,
            daily_limit: limit,
            worker_budget: None,
        };
        // This method has no middleware/tool dispatcher, even for tool-shaped responses.
        model.complete(request, cancellation).await
    }
}
