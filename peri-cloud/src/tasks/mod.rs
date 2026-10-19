//! Durable cloud workers. Task state, semantic notes, trace and user delivery are distinct.
pub(crate) mod approvals;
pub(crate) mod continuity;
mod evidence;
pub(crate) mod middleware;
pub(crate) mod recovery;
pub(crate) mod resources;
mod service;
pub(crate) mod sessions;
mod store;
mod suspension;
pub(crate) mod tools;
pub(crate) use service::WorkerService;

use crate::context::AgentRole;
use crate::gateway::ChannelRoute;
use serde::{Deserialize, Serialize};
use std::sync::Weak;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    #[error("task is missing or outside this conversation")]
    Forbidden,
    #[error("task input or transition is invalid")]
    Invalid,
    #[error("task changed; inspect the current revision before retrying")]
    Conflict,
    #[error("task capacity or trace retention limit reached")]
    Limit,
    #[error("task service is closing or unavailable")]
    Unavailable,
    #[error("task state storage is unavailable")]
    Database(#[from] sqlx::Error),
    #[error("task record is invalid")]
    Encoding(#[from] serde_json::Error),
}
pub(crate) type Result<T> = std::result::Result<T, TaskError>;

#[derive(Clone)]
pub(crate) struct TaskScope {
    pub service: Weak<WorkerService>,
    pub principal: Uuid,
    pub session: Uuid,
    pub root_session: Uuid,
    pub task: Option<Uuid>,
    pub role: AgentRole,
    pub execution_agent: Option<Uuid>,
    pub route: ChannelRoute,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct WorkItem {
    pub name: String,
    pub state: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct TaskRecord {
    pub id: Uuid,
    pub principal: Uuid,
    pub parent_session: Uuid,
    pub worker_session: Uuid,
    /// Stable execution conversation; worker_session identifies its physical window.
    #[serde(default)]
    pub execution_session: Uuid,
    pub parent_turn: Uuid,
    pub request_key: String,
    pub request_fingerprint: String,
    pub route: ChannelRoute,
    pub goal: String,
    pub reply_to: String,
    pub reply_kind: crate::gateway::ReplyKind,
    pub constraints: String,
    pub acceptance: String,
    pub state: String,
    pub turn: Option<Uuid>,
    #[serde(default)]
    pub run_number: u64,
    #[serde(default)]
    pub lifecycle_version: u8,
    #[serde(default)]
    pub wait_after: i64,
    #[serde(default)]
    pub wake_at: Option<i64>,
    pub revision: u64,
    pub note: String,
    pub plan: Vec<WorkItem>,
    pub result: String,
    pub notified: bool,
    pub privacy_epoch: u64,
    pub parent_generation: u64,
    pub permission_mode: u8,
    pub private_memory: bool,
    pub created_at: i64,
}
impl TaskRecord {
    pub fn terminal(&self) -> bool {
        matches!(
            self.state.as_str(),
            "completed" | "failed" | "cancelled" | "invalidated"
        )
    }
    pub fn run_key(&self) -> String {
        if self.run_number == 0 {
            format!("worker:{}", self.id)
        } else {
            format!("worker:{}:{}", self.id, self.run_number)
        }
    }
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Delegate {
    pub goal: String,
    #[serde(default)]
    pub constraints: String,
    #[serde(default)]
    pub acceptance: String,
    pub device_id: Option<Uuid>,
    pub workspace: Option<String>,
    /// Explicit reuse only. Device, canonical workspace and privacy must still match.
    pub execution_session: Option<Uuid>,
}
pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|v| v.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0)
}
pub(crate) fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}
