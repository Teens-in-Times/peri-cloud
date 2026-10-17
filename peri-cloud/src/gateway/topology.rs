//! Account-scoped presentation; configured gateways are not a claim of liveness.
use serde::Serialize;
use uuid::Uuid;

use super::{Gateway, GatewayError, GatewayResult};
use crate::identity::{Authenticated, IdentityError};

#[derive(Serialize)]
pub struct GatewayView {
    pub adapter: String,
    pub label: String,
    pub linked_identities: usize,
}

impl Gateway {
    pub async fn browser_gateways(&self, actor: &Authenticated) -> GatewayResult<Vec<GatewayView>> {
        let channels = self.identity.channels(actor).await?;
        let admin = self.identity.is_admin(actor).await?;
        let mut result: Vec<_> = self
            .adapters
            .iter()
            .filter_map(|(id, adapter)| {
                let linked = channels
                    .iter()
                    .filter(|c| c.adapter_instance_id == *id)
                    .count();
                (admin || linked > 0).then(|| GatewayView {
                    adapter: id.clone(),
                    label: adapter.display_name().into(),
                    linked_identities: linked,
                })
            })
            .collect();
        result.sort_by(|a, b| a.adapter.cmp(&b.adapter));
        Ok(result)
    }

    pub async fn browser_check_connection(
        &self,
        actor: &Authenticated,
        id: Uuid,
    ) -> GatewayResult<&'static str> {
        let devices = self.identity.devices(actor).await?;
        let device = devices
            .iter()
            .find(|d| d.id == id && !d.revoked)
            .ok_or(IdentityError::Forbidden)?;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(6),
            self.connector.check_connection(device),
        )
        .await
        .map_err(|_| GatewayError::Connection)?;
        // A revocation while the transport was being checked still wins.
        if !self
            .identity
            .devices(actor)
            .await?
            .iter()
            .any(|d| d.id == id && !d.revoked)
        {
            return Err(IdentityError::Forbidden.into());
        }
        result
    }
}
