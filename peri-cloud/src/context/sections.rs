//! Host-authored section manifest. This describes placement, never grants authority.
use super::ContextPart;
use serde::Serialize;
use sha2::{Digest, Sha256};
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SectionFreshness {
    Configuration,
    TurnSnapshot,
    ModelBoundary,
}
#[derive(Clone, Debug, Serialize)]
pub struct ContextSectionInfo {
    pub key: String,
    pub origin: String,
    pub freshness: SectionFreshness,
    pub bytes: usize,
    pub version: String,
    pub tool_count: usize,
    pub role: String,
}
impl ContextSectionInfo {
    pub(super) fn for_part(part: &ContextPart) -> Self {
        let (origin, freshness) = match part.key.as_str() {
            "base" => ("host configuration", SectionFreshness::Configuration),
            "role" => ("host role rules", SectionFreshness::Configuration),
            "soul" => (
                "assistant SOUL; user preferences subordinate to host",
                SectionFreshness::TurnSnapshot,
            ),
            "channel" => (
                "authenticated channel adapter",
                SectionFreshness::TurnSnapshot,
            ),
            "memory_protocol" => ("host memory protocol", SectionFreshness::Configuration),
            "files" => (
                "operator-approved context documents",
                SectionFreshness::TurnSnapshot,
            ),
            "tools" => (
                "registered middleware tool catalog",
                SectionFreshness::TurnSnapshot,
            ),
            "extensions" => (
                "host skills and MCP loading protocol",
                SectionFreshness::Configuration,
            ),
            _ => ("host extension", SectionFreshness::TurnSnapshot),
        };
        Self {
            key: part.key.clone(),
            origin: origin.into(),
            freshness,
            bytes: part.text.len(),
            version:format!("sha256:{}",(Sha256::digest(serde_json::to_vec(&serde_json::json!({"text":part.text,"tools":part.tools.iter().map(|tool|tool.definition()).collect::<Vec<_>>()})).unwrap_or_default())).iter().map(|byte| format!("{byte:02x}")).collect::<String>()),
            tool_count: part.tools.len(),
            role: "selected cloud role".into(),
        }
    }
}
