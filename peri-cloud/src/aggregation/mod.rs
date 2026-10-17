//! Deterministic adapter to the existing New API, independent of inference.
mod client;
mod management;
pub use management::{ChannelAction, ChannelCommand, ChannelFields};
mod selection;
pub(crate) use selection::migrate;
mod types;
use crate::identity::{Authenticated, ChannelIdentity, IdentityError, IdentityService};
use crate::state::CloudJournal;
use std::sync::Arc;
pub use types::*;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum AggregateError {
    #[error("invalid aggregate configuration or input")]
    Invalid,
    #[error("aggregate authorization rejected")]
    Identity(#[from] IdentityError),
    #[error("aggregate state unavailable")]
    Database(#[from] sqlx::Error),
    #[error("resource does not belong to this account")]
    Ownership,
    #[error("configuration has changed; reload before editing")]
    Conflict,
    #[error("aggregate request failed: {0}")]
    Upstream(&'static str),
    #[error("write outcome is unknown; inspect before retrying")]
    Unknown,
}
pub type AggregateResult<T> = Result<T, AggregateError>;

/// Captured by an authenticated transport. Never populated from model arguments.
pub enum ControlActor<'a> {
    Browser(&'a Authenticated),
    Channel(&'a ChannelIdentity),
}

pub struct Aggregation {
    client: client::Client,
    identity: Arc<IdentityService>,
    journal: Arc<CloudJournal>,
    default_model: String,
    max_tokens: u32,
    writes: tokio::sync::Mutex<()>,
}
impl Aggregation {
    pub fn new(
        config: &AggregationConfig,
        management_key: String,
        inference_key: String,
        identity: Arc<IdentityService>,
        journal: Arc<CloudJournal>,
        default_model: String,
        max_tokens: u32,
    ) -> AggregateResult<Arc<Self>> {
        config.validate()?;
        Ok(Arc::new(Self {
            client: client::Client::new(config, management_key, inference_key)?,
            identity,
            journal,
            default_model,
            max_tokens,
            writes: tokio::sync::Mutex::new(()),
        }))
    }
    async fn principal(&self, actor: &ControlActor<'_>, admin: bool) -> AggregateResult<Uuid> {
        let (principal, allowed) = match actor {
            ControlActor::Browser(actor) => {
                self.identity.browser_scope(actor).await?;
                (
                    actor.principal().id,
                    !admin || self.identity.is_admin(actor).await?,
                )
            }
            ControlActor::Channel(channel) => (
                self.identity.resolve_channel(channel).await?,
                !admin || self.identity.channel_is_admin(channel).await?,
            ),
        };
        if !allowed {
            return Err(IdentityError::Forbidden.into());
        }
        Ok(principal)
    }
    pub async fn models(&self, actor: &ControlActor<'_>) -> AggregateResult<Vec<String>> {
        self.principal(actor, false).await?;
        self.client.models().await
    }
    pub async fn channels(
        &self,
        actor: &ControlActor<'_>,
        page: usize,
    ) -> AggregateResult<ChannelPage> {
        self.principal(actor, true).await?;
        self.client.channels(page).await
    }
    pub async fn groups(&self, actor: &ControlActor<'_>) -> AggregateResult<Vec<String>> {
        self.principal(actor, true).await?;
        self.client.groups().await
    }
    pub fn inference_base(&self) -> String {
        self.client.inference_base().to_string()
    }
    pub async fn usage(&self, actor: &ControlActor<'_>) -> AggregateResult<serde_json::Value> {
        self.principal(actor, true).await?;
        let value = self
            .client
            .request(reqwest::Method::GET, "/api/usage/token/", None, true)
            .await?;
        let data = &value["data"];
        Ok(
            serde_json::json!({"total_used":data["total_used"].as_i64(),"total_available":data["total_available"].as_i64(),"unlimited_quota":data["unlimited_quota"].as_bool(),"expires_at":data["expires_at"].as_i64(),"unit":"new_api_quota"}),
        )
    }
    pub async fn audit(&self, actor: &ControlActor<'_>) -> AggregateResult<serde_json::Value> {
        self.principal(actor, true).await?;
        let rows=sqlx::query_as::<_,(String,String,String,String,i64)>("SELECT id,kind,target,state,created_at FROM control_operations ORDER BY created_at DESC,rowid DESC LIMIT 50").fetch_all(&self.journal.identity_pool()).await?;
        Ok(serde_json::Value::Array(rows.into_iter().map(|(id,kind,target,state,created_at)|serde_json::json!({"id":id,"kind":kind,"target":target,"state":state,"created_at":created_at})).collect()))
    }
}

#[cfg(test)]
mod tests;
