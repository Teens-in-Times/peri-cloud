//! Per-account credentials are write-only API inputs, never model context.
use crate::state::CloudJournal;
use peri_model::{Model, OpenAiConfig, OpenAiModel};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::sync::Arc;
use url::Url;
use uuid::Uuid;
#[path = "models_direct.rs"]
mod direct;
pub use direct::ModelProtocol;
pub(crate) use direct::{direct_model, valid_endpoint};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelProvider {
    pub id: String,
    pub name: String,
    pub api_base: Url,
    #[serde(default)]
    pub protocol: ModelProtocol,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelUpdate {
    pub provider: String,
    pub model: String,
    pub api_key: Option<String>,
    pub api_base: Option<Url>,
    pub protocol: Option<ModelProtocol>,
}

#[derive(Serialize)]
pub struct ModelChoice {
    pub provider: String,
    pub model: String,
    pub has_api_key: bool,
    pub api_base: Option<Url>,
    pub protocol: ModelProtocol,
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
    aggregation: parking_lot::RwLock<Option<Arc<crate::aggregation::Aggregation>>>,
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
            aggregation: parking_lot::RwLock::new(None),
        }))
    }
    pub fn set_aggregation(&self, value: Arc<crate::aggregation::Aggregation>) {
        *self.aggregation.write() = Some(value);
    }
    pub fn aggregation(&self) -> Option<Arc<crate::aggregation::Aggregation>> {
        self.aggregation.read().clone()
    }
    pub fn providers(&self) -> &[ModelProvider] {
        &self.providers
    }
    pub fn default_model(&self) -> &str {
        &self.default_model
    }
    pub async fn choice(&self, principal: Uuid) -> Result<Option<ModelChoice>, ModelConfigError> {
        if let Some(aggregation) = self.aggregation() {
            let model = aggregation
                .current_model(principal, None)
                .await
                .map_err(|_| ModelConfigError::Invalid)?;
            return Ok(Some(ModelChoice {
                provider: "gateway".into(),
                model,
                has_api_key: false,
                api_base: None,
                protocol: ModelProtocol::Openai,
            }));
        }
        let row = sqlx::query("SELECT provider,model FROM user_models WHERE principal=?")
            .bind(principal.to_string())
            .fetch_optional(&self.journal.identity_pool())
            .await?;
        let Some(row) = row else { return Ok(None) };
        let provider: String = row.get("provider");
        let endpoint = if provider == "@custom" {
            self.direct_endpoint(principal).await?
        } else {
            self.providers
                .iter()
                .find(|p| p.id == provider)
                .map(|p| direct::DirectEndpoint {
                    base: p.api_base.clone(),
                    protocol: p.protocol,
                })
        };
        Ok(Some(ModelChoice {
            provider,
            model: row.get("model"),
            has_api_key: true,
            api_base: endpoint.as_ref().map(|e| e.base.clone()),
            protocol: endpoint.map(|e| e.protocol).unwrap_or_default(),
        }))
    }
    pub async fn save(&self, principal: Uuid, input: ModelUpdate) -> Result<(), ModelConfigError> {
        if self.aggregation().is_some() {
            return Err(ModelConfigError::Invalid);
        }
        let custom = input.provider == "@custom";
        if (!custom && !self.providers.iter().any(|p| p.id == input.provider))
            || (!custom && (input.api_base.is_some() || input.protocol.is_some()))
            || input.model.trim().is_empty()
            || input.model.len() > 256
            || input.model.chars().any(char::is_control)
        {
            return Err(ModelConfigError::Invalid);
        }
        let previous = self.direct_endpoint(principal).await?;
        let endpoint = if custom {
            let base = input
                .api_base
                .clone()
                .or_else(|| previous.as_ref().map(|p| p.base.clone()))
                .ok_or(ModelConfigError::Invalid)?;
            let protocol = input
                .protocol
                .or_else(|| previous.as_ref().map(|p| p.protocol))
                .unwrap_or_default();
            if !valid_endpoint(&base) {
                return Err(ModelConfigError::Invalid);
            }
            if input.api_key.as_ref().is_none_or(|k| k.is_empty())
                && previous
                    .as_ref()
                    .is_none_or(|p| p.base != base || p.protocol != protocol)
            {
                return Err(ModelConfigError::Invalid);
            }
            Some(direct::DirectEndpoint { base, protocol })
        } else {
            None
        };
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
        if let Some(endpoint) = endpoint {
            sqlx::query("INSERT INTO model_endpoints(principal,api_base,protocol) VALUES(?,?,?) ON CONFLICT(principal) DO UPDATE SET api_base=excluded.api_base,protocol=excluded.protocol")
                .bind(principal.to_string()).bind(endpoint.base.as_str()).bind(endpoint.protocol.code()).execute(&mut *tx).await?;
        } else {
            sqlx::query("DELETE FROM model_endpoints WHERE principal=?")
                .bind(principal.to_string())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn reset(&self, principal: Uuid) -> Result<(), ModelConfigError> {
        if self.aggregation().is_some() {
            return Err(ModelConfigError::Invalid);
        }
        let mut tx = self.journal.identity_pool().begin().await?;
        sqlx::query("DELETE FROM model_endpoints WHERE principal=?")
            .bind(principal.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM user_models WHERE principal=?")
            .bind(principal.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    pub(crate) async fn resolve_for_turn(
        &self,
        principal: Uuid,
        session: Uuid,
        turn: Uuid,
        fallback: Arc<dyn Model>,
    ) -> Result<Arc<dyn Model>, ModelConfigError> {
        if let Some(aggregation) = self.aggregation() {
            return aggregation
                .model_for_turn(principal, Some(session), Some(turn))
                .await
                .map_err(|_| ModelConfigError::Invalid);
        }
        self.resolve(principal, Some(session), fallback).await
    }
    pub(crate) async fn resolve(
        &self,
        principal: Uuid,
        session: Option<Uuid>,
        fallback: Arc<dyn Model>,
    ) -> Result<Arc<dyn Model>, ModelConfigError> {
        if let Some(aggregation) = self.aggregation() {
            return aggregation
                .model_for(principal, session)
                .await
                .map_err(|_| ModelConfigError::Invalid);
        }
        let row = sqlx::query("SELECT provider,model,api_key FROM user_models WHERE principal=?")
            .bind(principal.to_string())
            .fetch_optional(&self.journal.identity_pool())
            .await?;
        let Some(row) = row else {
            return Ok(fallback);
        };
        let provider: String = row.get("provider");
        let endpoint = if provider == "@custom" {
            self.direct_endpoint(principal)
                .await?
                .ok_or(ModelConfigError::Invalid)?
        } else {
            let base = self
                .providers
                .iter()
                .find(|p| p.id == provider)
                .ok_or(ModelConfigError::Invalid)?;
            direct::DirectEndpoint {
                base: base.api_base.clone(),
                protocol: base.protocol,
            }
        };
        Ok(direct_model(
            endpoint.protocol,
            endpoint.base,
            row.get("api_key"),
            row.get("model"),
            self.max_tokens,
        ))
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
