//! Versioned device protocol. SSH and account secrets are never tool inputs.

use crate::tools::{ContextRetention, ToolDefinition, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutorInfo {
    pub protocol_version: u32,
    pub device_id: Uuid,
    pub device_name: String,
    pub platform: String,
    pub version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenSession {
    pub workspace: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionBinding {
    pub session_id: Uuid,
    pub workspace: String,
    pub workspace_identity: WorkspaceIdentity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceIdentity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NativeToolDescriptor {
    pub definition: ToolDefinition,
    pub aliases: Vec<String>,
    pub namespace: Option<String>,
    pub is_direct: bool,
    pub output_char_limit: Option<usize>,
    pub prefers_persist: bool,
    pub context_retention: ContextRetention,
    pub prompt_declaration: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitJob {
    pub invocation_id: Uuid,
    pub session_id: Uuid,
    pub tool: String,
    pub input: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
    RecoveryRequired,
}

impl JobStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::TimedOut
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobFailure {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobSnapshot {
    pub invocation_id: Uuid,
    pub session_id: Uuid,
    pub tool: String,
    pub status: JobStatus,
    pub cancel_requested: bool,
    pub submitted_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub pid: Option<u32>,
    pub stdout_path: Option<String>,
    pub stderr_path: Option<String>,
    pub output: Option<ToolOutput>,
    pub failure: Option<JobFailure>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobReceipt {
    pub created: bool,
    pub job: JobSnapshot,
}

#[cfg(test)]
#[path = "device_executor_test.rs"]
mod tests;
