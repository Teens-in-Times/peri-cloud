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
    /// Additive feature identifiers. Older executors omit this field.
    #[serde(default)]
    pub features: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenSession {
    pub workspace: String,
}

/// Account UI directory discovery; never a model tool or an execution request.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryQuery {
    pub path: String,
    #[serde(default)]
    pub offset: usize,
    pub limit: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirectoryEntry {
    pub name: String,
    pub path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirectoryListing {
    pub path: String,
    pub parent: Option<String>,
    pub roots: Vec<String>,
    pub directories: Vec<DirectoryEntry>,
    pub next_offset: Option<usize>,
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

/// Explicit operator attestation after inspecting the original device. Missing
/// admissions are durably fenced so a delayed submit cannot run afterwards.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveJob {
    pub session_id: Uuid,
    pub tool: String,
    pub note: String,
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

/// Feature advertised by executors that expose the binding-scoped workspace
/// read endpoints. The cloud denies the capability when it is absent instead
/// of probing endpoints that older executors do not serve.
pub const WORKSPACE_READ_FEATURE: &str = "workspace-read@1";
pub const ENVIRONMENT_READ_FEATURE: &str = "environment-read@1";

/// Authenticated observations, never permissions or proof of future liveness.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnvironmentSnapshot {
    pub device_id: Uuid,
    pub binding: SessionBinding,
    pub platform: String,
    pub architecture: String,
    pub shell: String,
    pub observed_at: String,
    pub applications: Vec<EnvironmentApplication>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnvironmentApplication {
    pub name: String,
    pub executable: Option<String>,
    /// Discovery does not launch binaries; versions need normal approved tools.
    pub version: Option<String>,
}

/// One workspace file returned by the device. `path` is workspace-relative and
/// always uses `/`; `digest` is `sha256:<64 lowercase hex>` over the bytes read.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceFile {
    pub path: String,
    pub content: String,
    pub digest: String,
}

/// Project instruction candidates resolved on the device. `main` is the first
/// non-blank of `AGENTS.md`, `CLAUDE.md`, `.claude/AGENTS.md`; `local` is
/// `CLAUDE.local.md`. `@import` expansion is not performed by this endpoint.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WorkspaceInstructions {
    pub main: Option<WorkspaceFile>,
    pub local: Option<WorkspaceFile>,
    /// Ordered cwd documents. Older peers can continue using main/local.
    #[serde(default)]
    pub documents: Vec<WorkspaceFile>,
}

/// `SKILL.md` paths found under `<workspace>/.claude/skills`, sorted.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WorkspaceSkills {
    pub paths: Vec<String>,
    pub truncated: bool,
}

/// Read one already-discoverable skill entry. The executor re-validates that
/// the path is under `.claude/skills` and is named `SKILL.md`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceReadRequest {
    pub path: String,
}

#[cfg(test)]
#[path = "device_executor_test.rs"]
mod tests;
