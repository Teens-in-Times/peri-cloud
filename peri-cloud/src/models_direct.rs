use super::*;

/// Protocol adapters shipped with Peri; an external aggregator is optional.
#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelProtocol {
    #[default]
    #[serde(alias = "openai_chat_completions")]
    Openai,
    #[serde(alias = "anthropic_messages")]
    Anthropic,
}
impl ModelProtocol {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
        }
    }
}
pub(super) struct DirectEndpoint {
    pub base: Url,
    pub protocol: ModelProtocol,
}
pub(crate) fn direct_model(
    protocol: ModelProtocol,
    base: Url,
    key: String,
    name: String,
    max_tokens: u32,
) -> Arc<dyn Model> {
    match protocol {
        ModelProtocol::Openai => Arc::new(OpenAiModel::new(
            OpenAiConfig::new(base, key, name).with_max_tokens(max_tokens),
        )),
        ModelProtocol::Anthropic => Arc::new(peri_model::AnthropicModel::new(
            peri_model::AnthropicConfig::new(base, key, name).with_max_tokens(max_tokens),
        )),
    }
}
pub(crate) fn valid_endpoint(url: &Url) -> bool {
    url.as_str().len() <= 4096
        && url.host().is_some()
        && (url.scheme() == "https"
            || (url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1" | "[::1]"))))
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}
impl AccountModels {
    pub(super) async fn direct_endpoint(
        &self,
        principal: Uuid,
    ) -> Result<Option<DirectEndpoint>, ModelConfigError> {
        let row = sqlx::query("SELECT api_base,protocol FROM model_endpoints WHERE principal=?")
            .bind(principal.to_string())
            .fetch_optional(&self.journal.identity_pool())
            .await?;
        row.map(|r| {
            Ok(DirectEndpoint {
                base: r
                    .get::<String, _>("api_base")
                    .parse()
                    .map_err(|_| ModelConfigError::Invalid)?,
                protocol: serde_json::from_value(serde_json::json!(r.get::<String, _>("protocol")))
                    .map_err(|_| ModelConfigError::Invalid)?,
            })
        })
        .transpose()
    }
}
