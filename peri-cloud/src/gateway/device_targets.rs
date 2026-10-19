//! Authenticated target preparation. Transport coordinates never cross into model input.
use super::*;
use crate::tasks::TaskScope;
use peri_remote_tools::RemoteSession;
use serde_json::{json, Value};

impl Gateway {
    pub(crate) async fn model_devices(&self, scope: &TaskScope) -> GatewayResult<Value> {
        if self.identity.resolve_channel(&scope.route.identity).await? != scope.principal
            || !self.route_allowed(&scope.route).await?
        {
            return Err(IdentityError::Forbidden.into());
        }
        let (_, devices) = self.identity.channel_devices(&scope.route.identity).await?;
        Ok(
            json!({"devices":devices.iter().filter(|d|!d.revoked).take(64).map(|d|json!({"device_id":d.id,"name":d.name,"platform":d.platform,"registration_is_not_online":true})).collect::<Vec<_>>(),"prepare_required":true}),
        )
    }
    pub(crate) async fn prepare_native_target(
        &self,
        scope: &TaskScope,
        device: Uuid,
        workspace: Option<&str>,
    ) -> GatewayResult<(crate::state::FrozenSession, Arc<RemoteSession>)> {
        if self.identity.resolve_channel(&scope.route.identity).await? != scope.principal
            || !self.route_allowed(&scope.route).await?
        {
            return Err(IdentityError::Forbidden.into());
        }
        let (_, devices) = self.identity.channel_devices(&scope.route.identity).await?;
        let target = devices
            .iter()
            .find(|d| d.id == device && !d.revoked)
            .ok_or(IdentityError::Forbidden)?;
        let agent = match workspace {
            Some(path) => {
                self.connector
                    .open_workspace(scope.principal, target, path)
                    .await?
            }
            None => self.connector.open(scope.principal, target, None).await?,
        };
        let mut frozen = agent.frozen_session(scope.principal);
        // The executor binding is independent of the source model session.
        // No child conversation or runtime turn is created for a small operation.
        frozen.session_id = scope.session;
        let remote = self.reopen_native_target(scope.principal, &frozen).await?;
        Ok((frozen, remote))
    }
    pub(crate) async fn reopen_native_target(
        &self,
        principal: Uuid,
        frozen: &crate::state::FrozenSession,
    ) -> GatewayResult<Arc<RemoteSession>> {
        let devices = crate::memory::scope::devices(&self.store.pool, principal).await?;
        let device = devices
            .iter()
            .find(|d| Some(d.id) == frozen.device_id && !d.revoked)
            .ok_or(IdentityError::Forbidden)?;
        self.connector
            .open(principal, device, Some(frozen))
            .await?
            .native_session()
            .ok_or(GatewayError::Connection)
    }
}
