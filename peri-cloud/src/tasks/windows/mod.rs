//! Host-owned window rotation. A handoff conveys work understanding, never authority.
pub(crate) mod budget;
mod coordinator;
mod handoff;
mod history;
pub(super) use history::HistoryFilter;
mod store;
pub(super) use store::active_parent_goal;
pub(super) use store::active_parent_goal_tx;
pub(crate) use store::migrate;
#[cfg(test)]
mod tests;

use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum RotationCause {
    #[default]
    Budget,
    Worker,
    Manual,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum RotationPhase {
    Requested,
    Summarizing,
    Prepared,
    Committed,
    Aborted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Rotation {
    pub id: Uuid,
    pub task: Uuid,
    pub source_window: Uuid,
    pub source_turn: Uuid,
    pub checkpoint_hash: String,
    pub instruction_watermark: i64,
    pub cause: RotationCause,
    pub prior_state: String,
    pub phase: RotationPhase,
    pub summary: Option<String>,
    pub summary_status: String,
    pub usage: Option<serde_json::Value>,
    pub destination: Option<Uuid>,
    pub created_at: i64,
}
