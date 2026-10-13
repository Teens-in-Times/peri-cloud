//! Per-account credentials are write-only API inputs, never model context.
use crate::state::CloudJournal;
use peri_model::{Model, OpenAiConfig, OpenAiModel};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::sync::Arc;
use url::Url;
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelProvider {
    pub id: String,
    pub name: String,
    pub api_base: Url,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelUpdate {
    pub provider: String,
    pub model: String,
    pub api_key: Option<String>,
}

#[derive(Serialize)]
pub struct ModelChoice {
    pub provider: String,
    pub model: String,
    pub has_api_key: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ModelConfigError {
    #[error("model configuration is invalid")]
    Invalid,
    #[error("model configuration state is unavailable")]
    State(#[from] sqlx::Error),
}

pub struct AccountModels {
    journal: Arc<CloudJournal>,
    providers: Vec<ModelProvider>,
    default_model: String,
    max_tokens: u32,
}

impl AccountModels {
    pub fn new(
        journal: Arc<CloudJournal>,
        providers: Vec<ModelProvider>,
        default_model: String,
        max_tokens: u32,
    ) -> Result<Arc<Self>, ModelConfigError> {
        let mut ids = std::collections::HashSet::new();
        if providers.is_empty()
            || providers.len() > 32
            || providers
                .iter()
                .any(|p| !valid_provider(p) || !ids.insert(&p.id))
        {
            return Err(ModelConfigError::Invalid);
        }
        Ok(Arc::new(Self {
            journal,
            providers,
            default_model,
            max_tokens,
        }))
    }
    pub fn providers(&self) -> &[ModelProvider] {
        &self.providers
    }
    pub fn default_model(&self) -> &str {
        &self.default_model
    }
    pub async fn choice(&self, principal: Uuid) -> Result<Option<ModelChoice>, ModelConfigError> {
        let row = sqlx::query("SELECT provider,model FROM user_models WHERE principal=?")
            .bind(principal.to_string())
            .fetch_optional(&self.journal.identity_pool())
            .await?;
        Ok(row.map(|r| ModelChoice {
            provider: r.get("provider"),
            model: r.get("model"),
            has_api_key: true,
        }))
    }
    pub async fn save(&self, principal: Uuid, input: ModelUpdate) -> Result<(), ModelConfigError> {
        if !self.providers.iter().any(|p| p.id == input.provider)
            || input.model.trim().is_empty()
            || input.model.len() > 256
            || input.model.chars().any(char::is_control)
        {
            return Err(ModelConfigError::Invalid);
        }
        let pool = self.journal.identity_pool();
        let mut tx = pool.begin().await?;
        let key = match input.api_key.filter(|s| !s.is_empty()) {
            Some(key) if key.len() <= 8192 && !key.chars().any(char::is_control) => key,
            Some(_) => return Err(ModelConfigError::Invalid),
            None => {
                let row =
                    sqlx::query("SELECT api_key FROM user_models WHERE principal=? AND provider=?")
                        .bind(principal.to_string())
                        .bind(&input.provider)
                        .fetch_optional(&mut *tx)
                        .await?
                        .ok_or(ModelConfigError::Invalid)?;
                row.get("api_key")
            }
        };
        sqlx::query("INSERT INTO user_models(principal,provider,model,api_key) VALUES(?,?,?,?) ON CONFLICT(principal) DO UPDATE SET provider=excluded.provider,model=excluded.model,api_key=excluded.api_key")
            .bind(principal.to_string()).bind(input.provider).bind(input.model.trim()).bind(key).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn reset(&self, principal: Uuid) -> Result<(), ModelConfigError> {
        sqlx::query("DELETE FROM user_models WHERE principal=?")
            .bind(principal.to_string())
            .execute(&self.journal.identity_pool())
            .await?;
        Ok(())
    }
    pub(crate) async fn resolve(
        &self,
        principal: Uuid,
        fallback: Arc<dyn Model>,
    ) -> Result<Arc<dyn Model>, ModelConfigError> {
        let row = sqlx::query("SELECT provider,model,api_key FROM user_models WHERE principal=?")
            .bind(principal.to_string())
            .fetch_optional(&self.journal.identity_pool())
            .await?;
        let Some(row) = row else {
            return Ok(fallback);
        };
        let provider: String = row.get("provider");
        let base = self
            .providers
            .iter()
            .find(|p| p.id == provider)
            .ok_or(ModelConfigError::Invalid)?;
        Ok(Arc::new(OpenAiModel::new(
            OpenAiConfig::new(
                base.api_base.clone(),
                row.get::<String, _>("api_key"),
                row.get::<String, _>("model"),
            )
            .with_max_tokens(self.max_tokens),
        )))
    }
}

pub fn valid_provider(p: &ModelProvider) -> bool {
    let url = &p.api_base;
    !p.id.is_empty()
        && p.id.len() <= 64
        && p.id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        && !p.name.trim().is_empty()
        && p.name.len() <= 128
        && !p.name.chars().any(char::is_control)
        && url.host().is_some()
        && (url.scheme() == "https"
            || (url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1" | "[::1]"))))
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}

#[cfg(test)]
#[path = "models_test.rs"]
mod tests;
