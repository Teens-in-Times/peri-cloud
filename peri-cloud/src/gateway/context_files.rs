use super::{Gateway, GatewayError, GatewayResult};
use crate::context::ContextDocument;
use crate::identity::Authenticated;

impl Gateway {
    pub async fn context_documents(
        &self,
        actor: &Authenticated,
    ) -> GatewayResult<Vec<ContextDocument>> {
        self.identity.browser_scope(actor).await?;
        let assembler = self
            .connector
            .context_assembler()
            .ok_or(GatewayError::Invalid)?;
        let files = assembler.files().ok_or(GatewayError::Invalid)?;
        let text = files
            .read_user(actor.principal().id, "SOUL.md")
            .await
            .map_err(|_| GatewayError::Invalid)?
            .unwrap_or_default();
        Ok(vec![ContextDocument {
            name: "SOUL.md".into(),
            text,
        }])
    }
    pub async fn save_context_document(
        &self,
        actor: &Authenticated,
        document: ContextDocument,
    ) -> GatewayResult<()> {
        self.identity.browser_scope(actor).await?;
        let gate = self.account_gate(actor.principal().id.to_string());
        let _guard = gate.lock().await;
        self.identity.browser_scope(actor).await?;
        let assembler = self
            .connector
            .context_assembler()
            .ok_or(GatewayError::Invalid)?;
        assembler
            .files()
            .ok_or(GatewayError::Invalid)?
            .write_user(actor.principal().id, document)
            .await
            .map_err(|_| GatewayError::Invalid)
    }
}
