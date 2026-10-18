//! Account facts. Scheduling and device transports never own a copy of memory.
pub(crate) mod context;
pub(crate) mod frames;
mod migration;
pub(crate) mod scope;
mod search;
mod store;
pub(crate) mod tools;
mod upgrade;

pub(crate) use migration::migrate;
pub use scope::MemoryScope;
pub use store::MemoryStore;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MAX_ITEMS: usize = 128;
pub const MAX_ITEM_BYTES: usize = 8192;
pub const MAX_QUERY_BYTES: usize = 512;
pub const SNAPSHOT_BYTES: usize = 6000;
pub const SNAPSHOT_CHARS: usize = 1200;
pub const RESULT_BYTES: usize = 24 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("memory scope is not authorized or supported")]
    Forbidden,
    #[error("invalid memory request")]
    Invalid,
    #[error("memory changed; reload its revision and generation")]
    Conflict,
    #[error("memory was not found or is no longer available")]
    NotFound,
    #[error("memory capacity exceeded")]
    Limit,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Encoding(#[from] serde_json::Error),
}
pub type MemoryResult<T> = Result<T, MemoryError>;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    #[default]
    Fact,
    Decision,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryContent {
    pub content: String,
    #[serde(default)]
    pub kind: MemoryKind,
    #[serde(default)]
    pub pinned: bool,
    pub valid_until: Option<i64>,
}

/// Constructed by authenticated entry points, never deserialized from a tool.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemorySource {
    pub kind: String,
    pub session_id: Option<Uuid>,
    pub turn_id: Option<Uuid>,
    pub event_key: Option<String>,
}
impl MemorySource {
    pub fn web() -> Self {
        Self::entry("web")
    }
    pub(crate) fn entry(kind: &str) -> Self {
        Self {
            kind: kind.into(),
            session_id: None,
            turn_id: None,
            event_key: None,
        }
    }
    pub(crate) fn agent(session_id: Uuid, turn_id: Uuid) -> Self {
        Self {
            session_id: Some(session_id),
            turn_id: Some(turn_id),
            ..Self::entry("agent")
        }
    }
    pub(crate) fn qq(session_id: Option<Uuid>, event_key: String) -> Self {
        Self {
            session_id,
            event_key: Some(event_key),
            ..Self::entry("qq")
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryItem {
    pub id: Uuid,
    pub revision: u64,
    pub scope: MemoryScope,
    #[serde(flatten)]
    pub value: MemoryContent,
    pub source: MemorySource,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MemoryPage {
    pub generation: u64,
    pub total: usize,
    pub used_bytes: usize,
    pub items: Vec<MemoryItem>,
    pub has_more: bool,
    pub next_offset: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct MemorySnapshot {
    pub generation: u64,
    pub items: Vec<MemoryItem>,
    pub omitted: usize,
}

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}

#[cfg(test)]
mod frames_test;
#[cfg(test)]
mod scope_test;
#[cfg(test)]
mod tests;
