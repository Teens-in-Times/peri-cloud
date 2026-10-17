use super::{AggregateError, AggregateResult};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use url::Url;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AggregationConfig {
    pub origin: Url,
    pub management_key_file: PathBuf,
    pub inference_key_file: PathBuf,
}
impl AggregationConfig {
    pub fn validate(&self) -> AggregateResult<()> {
        let u = &self.origin;
        if u.scheme() != "http"
            || !matches!(u.host_str(), Some("127.0.0.1" | "[::1]"))
            || u.path() != "/"
            || !u.username().is_empty()
            || u.password().is_some()
            || u.query().is_some()
            || u.fragment().is_some()
            || self.management_key_file.as_os_str().is_empty()
            || self.inference_key_file.as_os_str().is_empty()
        {
            return Err(AggregateError::Invalid);
        }
        Ok(())
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct ChannelView {
    pub id: i64,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: i64,
    pub status: i64,
    pub models: String,
    pub group: String,
    pub priority: i64,
    pub weight: i64,
    pub base_url: Option<String>,
    pub model_mapping: Option<String>,
    pub test_time: i64,
    pub response_time: i64,
    #[serde(default)]
    pub revision: String,
}
#[derive(Serialize)]
pub struct ChannelPage {
    pub items: Vec<ChannelView>,
    pub page: usize,
    pub total: usize,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSelection {
    pub model: Option<String>,
    pub revision: i64,
}
