//! Implementation leaf for `crate::agent::async_tasks::registry`.

use super::{projection, settlement};
pub(super) use std::collections::{HashMap, HashSet};
pub(super) use std::fmt::{self, Debug, Formatter};
pub(super) use std::sync::Arc;

pub(super) use peri_acp_types::tasks::{
    BgRegistryEvent, BgTaskKind, ExternalCancelFn, ExternalNotifyFn, TaskChange, TaskRecord,
    TaskRegistrationTicket, TaskSnapshot,
};
pub(super) use thiserror::Error;
pub(super) use tokio_util::sync::CancellationToken;
pub(super) use tracing::warn;

pub(super) use crate::agent::events::BackgroundTaskResult;

pub(super) use super::projection::TaskProjection;
pub(super) use super::registration::RegistrationReceipt;
pub(super) use crate::agent::async_tasks::agent_inbox::{
    BackgroundAgentInbox, QueuedSubagentMessage, SubagentMessageError,
};

/// 后台任务注册表错误（结构化，取代 String 错误）
///
/// 实现 `std::error::Error`，调用方可通过 `?` 自动转 `Box<dyn Error>` /
/// `anyhow::Error`。
#[derive(Debug, Error)]
pub enum BackgroundRegistryError {
    #[error("Session execution scope is closing")]
    Closing,
    #[error("Maximum {0} concurrent background tasks reached")]
    ConcurrentLimit(usize),
    #[error("Task {0} not found")]
    TaskNotFound(String),
    #[error("Task {0} is completing")]
    TaskCompleting(String),
    #[error("Task {0} cannot be cancelled: kill handle unavailable")]
    KillUnavailable(String),
    #[error("Task {0} already exists")]
    DuplicateTask(String),
    #[error("Task {0} registration ticket does not belong to this execution")]
    InvalidRegistration(String),
    #[error("Task {0} has already started execution")]
    TaskAlreadyStarted(String),
    #[error("External task {0} source identity cannot change")]
    ExternalIdentityConflict(String),
    #[error("Task {task_id} completion delivery failed: {reason}")]
    DeliveryFailed { task_id: String, reason: String },
    #[error("Task {0} requires asynchronous cancellation")]
    ExternalCancelRequiresAsync(String),
    #[error("External owner rejected cancellation for task {0}")]
    ExternalCancelFailed(String),
    #[error("External task has an invalid creation timestamp")]
    InvalidStartedAt,
    #[error(
        "External task {task_id} initiator conflict: recorded {recorded}, requested {requested}"
    )]
    ExternalInitiatorConflict {
        task_id: String,
        recorded: String,
        requested: String,
    },
    #[error("External task {0} is Unroutable: initiator is unknown")]
    ExternalUnroutable(String),
    #[error("External task owner handle `{service}` does not match kind `{kind}`")]
    ExternalOwnerKindMismatch { service: String, kind: String },
    #[error("Kind concurrent limit reached: {kind} ({current}/{limit})")]
    KindConcurrentLimit {
        kind: String,
        current: usize,
        limit: usize,
    },
}

pub enum BgCancelHandle {
    /// bg agent：取消 tokio task。
    /// 持 `JoinHandle`（而非 `AbortHandle`）——取消时先 `token.cancel()` 让任务
    /// 优雅退出，再 await JoinHandle 等待其走完收尾，超时才 abort。
    Abort(tokio::task::JoinHandle<()>),
    /// workflow：kill 闭包——转发到 `WorkflowTaskRegistry::kill`（真正的 kill_tx 在其内部）。
    /// `None` 表示 kill 通道不可用（如 spawn 失败），此时 `cancel()` 返回明确错误
    /// 而非假装成功（issue 2026-08-05：Workflow 取消无效）。
    Kill(Option<Box<dyn FnOnce() + Send + Sync>>),
    External {
        cancel: ExternalCancelFn,
        on_terminal: ExternalNotifyFn,
    },
}

impl Debug for BgCancelHandle {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            BgCancelHandle::Abort(_) => f.write_str("Abort(_)"),
            BgCancelHandle::Kill(_) => f.write_str("Kill(_)"),
            BgCancelHandle::External { .. } => f.write_str("External { .. }"),
        }
    }
}

/// 后台任务信息（注册表条目）
pub struct BackgroundTask {
    pub id: String,
    pub agent_name: String,
    pub prompt_summary: String,
    pub status: BackgroundTaskStatus,
    pub started_at: std::time::Instant,
    /// 任务创建时间（chrono UTC），用于 list_tasks_full().started_at 返回真实时间
    pub chrono_started_at: chrono::DateTime<chrono::Utc>,
    /// 任务类型
    pub kind: BgTaskKind,
    /// 按 kind 分发的取消句柄
    pub cancel_handle: BgCancelHandle,
    /// 取消令牌（仅 Agent 类任务）：cancel() 时先 `token.cancel()` 让工具层取消链
    /// 生效（run_react_loop 的 await 点响应后走完整收尾），超时再 abort 兜底。
    /// Shell/Workflow 类任务为 None（取消走 Pid/Kill 句柄）。
    pub cancel_token: Option<CancellationToken>,
    /// OS 进程 PID（仅 bg shell 有效）
    pub pid: Option<u32>,
    /// 输出预览（completed 时写入，最多 500 字符）
    pub output_preview: Option<String>,
    /// Present only for live background sub-agents; revoked before terminal events.
    pub agent_inbox: Option<Arc<BackgroundAgentInbox>>,
    /// 投递归属（直接发起会话）。`None` = 本地 owner 任务或测试构造。
    pub initiator_session_id: Option<String>,
    /// 执行 scope owner（root 会话）：有界等待到期时交接记录里的 scope owner。
    pub owner_session_id: Option<String>,
    /// Owner 身份（MCP 实例身份等），交接记录用。
    pub owner_identity: Option<String>,
}

/// 后台任务状态
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BackgroundTaskStatus {
    Running,
    /// Completion has been claimed; callbacks run outside the registry lock.
    Completing,
    Completed,
    Failed,
}

/// 后台任务信息 DTO（序列化用）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BgTaskInfo {
    pub task_id: String,
    pub kind: BgTaskKind,
    pub summary: String,
    pub status: BackgroundTaskStatus,
    pub started_at: String,
    pub duration_ms: u64,
    pub pid: Option<u32>,
    pub output_preview: Option<String>,
}

/// 后台任务注册中心
pub struct BackgroundTaskRegistry {
    pub(super) tasks: parking_lot::Mutex<HashMap<String, BackgroundTask>>,
    pub(super) registration_owner: uuid::Uuid,
    pub(super) registrations: parking_lot::Mutex<HashMap<String, Arc<RegistrationReceipt>>>,
    pub(super) projection: parking_lot::Mutex<TaskProjection>,
    pub(super) changes: tokio::sync::broadcast::Sender<TaskChange>,
    /// Derived wake signal for consumers waiting on task lifecycle changes.
    /// The registry remains the sole source of task state; this version is only
    /// a retained notification channel for re-checking that state.
    pub(super) activity_version: tokio::sync::watch::Sender<u64>,
    pub(super) event_sender:
        parking_lot::RwLock<Option<tokio::sync::mpsc::UnboundedSender<BgRegistryEvent>>>,
    pub(super) session_id: parking_lot::RwLock<String>,
    pub(in crate::agent::async_tasks) scope: Arc<crate::agent::async_tasks::scope::ExecutionScope>,
    pub(super) unsettled_external: parking_lot::Mutex<HashSet<String>>,
    pub(super) cancelled_tasks_waiting_cleanup: parking_lot::Mutex<HashMap<String, BgTaskKind>>,
    pub(super) pending_deliveries:
        parking_lot::Mutex<HashMap<String, settlement::PendingTaskDelivery>>,
    pub(super) delivery_worker_running: parking_lot::Mutex<bool>,
    pub(super) settlements_in_flight: std::sync::atomic::AtomicUsize,
}

pub(super) enum TaskRegistration {
    Created,
    PendingStart,
    TerminalRecovery,
}

impl Default for BackgroundTaskRegistry {
    fn default() -> Self {
        Self::new()
    }
}

include!("background_task_registry_section_2.rs");
