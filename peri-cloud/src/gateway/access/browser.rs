use super::{AccessChange, AccessRecord, GatewayAccess};
use crate::gateway::{Gateway, GatewayError, GatewayResult};
use crate::identity::Authenticated;

impl Gateway {
    /// Authenticated web recovery remains available even if all chat sources
    /// were deliberately blocked. Only the deployment administrator can use it.
    pub async fn browser_access(&self, actor: &Authenticated) -> GatewayResult<Vec<GatewayAccess>> {
        self.identity.require_admin(actor).await?;
        let mut adapters: Vec<_> = self.adapters.keys().cloned().collect();
        adapters.sort();
        let mut records = Vec::new();
        for adapter in adapters {
            records.push(GatewayAccess {
                record: self.store.access_policy(&adapter).await?,
                adapter,
            });
        }
        Ok(records)
    }
    pub async fn change_browser_access(
        &self,
        actor: &Authenticated,
        adapter: &str,
        revision: u64,
        change: &AccessChange,
    ) -> GatewayResult<AccessRecord> {
        let _write = self.access_gate.write().await;
        self.identity.require_admin(actor).await?;
        let target = self.adapters.get(adapter).ok_or(GatewayError::Invalid)?;
        if let AccessChange::Add { list, id } | AccessChange::Remove { list, id } = change {
            if !target.valid_access_id(*list, id) {
                return Err(GatewayError::Invalid);
            }
        }
        let current = self.store.access_policy(adapter).await?;
        if current.revision != revision {
            return Err(crate::state::StateError::InvocationConflict.into());
        }
        let next = current.policy.changed(change)?;
        self.store
            .save_access(
                actor.principal().id,
                adapter,
                revision,
                &next,
                self.identity.now(),
            )
            .await?;
        self.store.access_policy(adapter).await
    }
}
