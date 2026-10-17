//! The browser's internal task identity is derived from its authenticated owner.
use super::{Authenticated, ChannelIdentity, IdentityError, IdentityResult, IdentityService};
impl IdentityService {
    pub(crate) async fn web_identity(
        &self,
        actor: &Authenticated,
    ) -> IdentityResult<ChannelIdentity> {
        self.browser_scope(actor).await?;
        let id = actor.principal().id.to_string();
        sqlx::query("INSERT INTO channel_identities(adapter_instance,external_user,principal,revoked) VALUES(?,?,?,0) ON CONFLICT(adapter_instance,external_user) DO UPDATE SET revoked=0 WHERE principal=excluded.principal")
            .bind(crate::gateway::WEB_ADAPTER).bind(&id).bind(&id).execute(&self.pool).await?;
        let identity = ChannelIdentity {
            adapter_instance_id: crate::gateway::WEB_ADAPTER.into(),
            external_user_id: id,
        };
        if self.resolve_channel(&identity).await? != actor.principal().id {
            return Err(IdentityError::Forbidden);
        }
        Ok(identity)
    }
}
