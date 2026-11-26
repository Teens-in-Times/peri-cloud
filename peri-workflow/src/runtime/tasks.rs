//! Workflow 任务表：agent / tool 任务的发布、状态机、单次终态提交（CAS）与任务事件。
//!
//! 两种任务共用同一张表、同一 claim / 取消 / 事件路径（不建第二张表）：`agent`
//! 载荷来自引擎 `agent()`，`tool` 载荷来自脚本 `tool()` 的宿主回填通道。差别在
//! payload、终态载荷与准入计数（tool 任务不占 agent 槽位，也不计 tool/token 用量）。
//!
//! 任务表是 per-session、process-local 的运行期状态（不写 journal）；跨进程恢复
//! 不在范围（design §5.1）。键为 `(runId, taskId)`：`taskId` 作用域 = 单次 run，
//! 宿主不假设跨 run 唯一。

use std::collections::HashMap;
use std::fmt::{self, Debug, Formatter};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::protocol::{AgentRunParams, AgentRunResult, WorkflowToolOutcome};

use crate::runtime::close_gate::CloseGate;
use crate::runtime::event_log::BoundedEventLog;
use crate::runtime::WorkflowResourceUpdate;

/// 终态任务在快照表中的保留窗口（与 progress store 的 run 保留窗口同量级）。
const TASK_RETENTION_MS: i64 = 300_000;
/// 快照表硬上界：超出时从最旧的终态任务开始淘汰（活跃任务不淘汰）。
const MAX_TASK_RECORDS: usize = 4096;

/// 任务基线：未终态任务集合 + 同一临界区读取的已提交上界。
///
/// 资源面随任务事件页返回该基线（`activeTasks` / `headSequence` / `truncatedBefore`），
/// 作为消费者「全量扫描未终态任务」的唯一入口；其正确性依赖「快照与上界同临界区、
/// 上界不因淘汰回退」两条不变量（design §4.3；对抗审查 F3/F4）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskBaseline {
    /// 未终态任务（发布顺序；活跃任务不参与保留窗口淘汰）。
    pub active_tasks: Vec<WorkflowTaskRecord>,
    /// 基线的已提交上界：`sequence <= head_sequence` 的状态变更都在快照中可见。
    pub head_sequence: u64,
    /// 事件保留边界：`sequence < truncated_before` 的事件已不可读。
    pub truncated_before: u64,
}

/// 任务状态机：`published → executing → completed | failed | cancelled`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowTaskState {
    Published,
    Executing,
    Completed,
    Failed,
    Cancelled,
}

impl WorkflowTaskState {
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// 任务种类：引擎 agent 槽位（`agent()`）或脚本工具调用（`tool()`）。
///
/// 两种任务共用同一张任务表、同一 claim / 终态 CAS / 取消 / 事件路径；差别只在
/// payload、终态载荷与准入计数（tool 任务不占用 agent 槽位，也不计 tool/token 用量）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowTaskKind {
    Agent,
    Tool,
}

/// 任务发布载荷（tagged `kind`：投影里直接可见任务种类）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "kind"
)]
pub enum WorkflowTaskPayload {
    #[serde(rename = "agent")]
    Agent {
        #[serde(flatten)]
        params: AgentRunParams,
    },
    #[serde(rename = "tool")]
    Tool {
        /// 脚本给出的 effective 工具名（宿主在会话可见视图内解析）。
        tool_name: String,
        /// 已通过对象形状检查的调用参数（schema 校验归 MCP 服务端）。
        #[serde(default)]
        arguments: serde_json::Map<String, serde_json::Value>,
        /// run 内脚本工具调用连续序号（从 0 起；重放前缀按它连续匹配）。
        tool_seq: u64,
        /// 该调用发生前已完成的 `agent()` 调用数（重放边界）。
        agent_seq: u64,
    },
}

impl WorkflowTaskPayload {
    #[must_use]
    pub fn kind(&self) -> WorkflowTaskKind {
        match self {
            Self::Agent { .. } => WorkflowTaskKind::Agent,
            Self::Tool { .. } => WorkflowTaskKind::Tool,
        }
    }

    #[must_use]
    pub fn agent_params(&self) -> Option<&AgentRunParams> {
        match self {
            Self::Agent { params } => Some(params),
            Self::Tool { .. } => None,
        }
    }

    #[must_use]
    pub fn tool_call(&self) -> Option<(&str, &serde_json::Map<String, serde_json::Value>)> {
        match self {
            Self::Tool {
                tool_name,
                arguments,
                ..
            } => Some((tool_name.as_str(), arguments)),
            Self::Agent { .. } => None,
        }
    }

    /// `(toolSeq, agentSeq)`：脚本工具调用的确定性序号；agent 任务返回 `None`。
    #[must_use]
    pub fn tool_sequence(&self) -> Option<(u64, u64)> {
        match self {
            Self::Tool {
                tool_seq,
                agent_seq,
                ..
            } => Some((*tool_seq, *agent_seq)),
            Self::Agent { .. } => None,
        }
    }
}

/// 任务记录：发布参数 + 当前状态 + 终态结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowTaskRecord {
    pub task_id: String,
    pub run_id: String,
    /// agent 任务的引擎槽位身份；tool 任务没有槽位（省略，不伪造 `0`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<u64>,
    pub state: WorkflowTaskState,
    /// 发布时的完整参数快照（不可变；payload 的 tag 即任务种类）。
    pub params: WorkflowTaskPayload,
    /// 发布时刻（wall clock，毫秒）。
    pub published_at_ms: i64,
    /// agent 任务的终态结果（tool 任务为 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<AgentRunResult>,
    /// tool 任务的宿主回填终态（agent 任务为 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_outcome: Option<WorkflowToolOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl WorkflowTaskRecord {
    #[must_use]
    pub fn kind(&self) -> WorkflowTaskKind {
        self.params.kind()
    }

    #[must_use]
    pub fn is_tool(&self) -> bool {
        self.kind() == WorkflowTaskKind::Tool
    }
}

/// 发布结果：首个发布提交；重复发布幂等返回既有记录（不覆盖参数）。
///
/// `run` / `agent` 已被 kill（kill 栅栏先于发布到达）时同样提交，但记录直接落
/// `cancelled` 终态：事件流先 `published` 后 `cancelled`，消费者不得执行它。
///
/// 实例已关闭时发布被**拒绝**（[`PublishOutcome::Closed`]）：不登记记录、不追加
/// 事件（关闭顺序第 ① 步；与 run 准入共享同一关闭门）。
#[derive(Debug)]
pub enum PublishOutcome {
    Published(WorkflowTaskRecord),
    AlreadyKnown(WorkflowTaskRecord),
    /// 实例已关闭：关闭返回后到达的发布一律拒绝（对抗审查 F6）。
    Closed,
}

/// 执行 claim 结果：只有 [`ClaimOutcome::Claimed`] 允许启动执行（唯一 claim）。
///
/// 其他分支说明消费者重试 / kill 先到：调用方不得执行该任务。
#[derive(Debug)]
pub enum ClaimOutcome {
    /// 本次成功 claim：`published → executing`，取消句柄已登记。
    Claimed(WorkflowTaskRecord),
    /// 已被另一个消费者 claim（或有并发 claim 竞争失败）：不得重复执行。
    AlreadyExecuting(WorkflowTaskRecord),
    /// 任务已终态（kill 先于消费 / 已结算）：不得执行。
    AlreadyTerminal(WorkflowTaskRecord),
    /// 任务不存在。
    NotFound,
}

/// 终态提交的准入条件（允许结算的活跃态集合，单一权威）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleAdmission {
    /// 任意活跃态（`published` / `executing`）：内部取消路径使用（kill 覆盖排队与执行中）。
    AnyActive,
    /// 仅 `executing`：外部回填路径使用（回填只接受已被唯一 claim 的任务）。
    ExecutingOnly,
}

/// 终态提交结果（单次终态提交，compare-and-set）。
#[derive(Debug)]
pub enum SettleOutcome {
    /// 本次提交成功进入终态。
    Settled(WorkflowTaskRecord),
    /// 任务已有终态：返回既有记录（重复回填幂等；调用方对结果冲突记录告警）。
    AlreadyTerminal(WorkflowTaskRecord),
    /// 调用方传入非终态 `state`：拒绝提交，记录未变更（调用方 bug，不得静默写入）。
    InvalidState(WorkflowTaskRecord),
    /// 活跃态不满足准入条件（如回填 `published` 任务）：拒绝提交，记录未变更。
    NotAdmitted(WorkflowTaskRecord),
    /// 任务不存在（未知 taskId）。
    NotFound,
}

/// 任务事件类型（`workflow://tasks` 与 `workflow://runs/{runId}/tasks`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowTaskEventKind {
    Published,
    Executing,
    Completed,
    Failed,
    Cancelled,
}

impl WorkflowTaskEventKind {
    /// 稳定事件名（`eventId` 与 serde 序列化共用的唯一字面量；不得用 `Debug`）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Published => "published",
            Self::Executing => "executing",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

impl From<WorkflowTaskState> for WorkflowTaskEventKind {
    fn from(state: WorkflowTaskState) -> Self {
        match state {
            WorkflowTaskState::Published => Self::Published,
            WorkflowTaskState::Executing => Self::Executing,
            WorkflowTaskState::Completed => Self::Completed,
            WorkflowTaskState::Failed => Self::Failed,
            WorkflowTaskState::Cancelled => Self::Cancelled,
        }
    }
}

/// 任务事件：追加不可变，携带事件时刻的任务快照。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowTaskEvent {
    /// 单调 sequence（session 级任务流）。
    pub sequence: u64,
    /// 幂等键：`{runId}:{taskId}:{kind}`。
    pub event_id: String,
    pub run_id: String,
    pub task_id: String,
    /// agent 任务的槽位身份；tool 任务省略。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<u64>,
    pub kind: WorkflowTaskEventKind,
    pub at_ms: i64,
    /// 事件时刻的任务快照（发布事件含参数；终态事件含结果）。
    pub task: WorkflowTaskRecord,
}

/// 终态通知面：任务进入终态（新提交或观察到既有终态）后回调。
///
/// 语义（design §5.1/§5.2 第 4 条）：**先提交任务表，再投递结果**——回调发生在
/// 状态与事件提交之后，调用方（`WorkflowRuntime`）据此向 Node 投递 `agent/result`。
///
/// 契约：回调在任务表临界区内被调用（`commit_locked` / 幂等分支），实现必须
/// **非阻塞**（只做移交，例如 spawn 投递任务），且不得回调进入任务表（无重入）。
pub type TerminalSink = Arc<dyn Fn(&WorkflowTaskRecord) + Send + Sync>;

// The table lock serializes the first terminal transition and later deliveries.
type TerminalHandler = Arc<dyn Fn(&WorkflowTaskRecord, bool) + Send + Sync>;

struct TableInner {
    tasks: HashMap<(String, String), WorkflowTaskEntry>,
    /// 发布顺序（快照排序稳定）。
    order: Vec<(String, String)>,
    /// run 级 kill 栅栏：置位后该 run 新发布的任务直接转 `cancelled`（值 = 取消原因）。
    run_kills: HashMap<String, Option<String>>,
    /// agent 级 kill 栅栏：置位后该 `(run, agent)` 新发布的任务直接转 `cancelled`。
    agent_kills: HashMap<(String, u64), Option<String>>,
    /// 出现过不可确认脚本工具副作用的 run（tool 终态 `outcome_unknown` /
    /// `task_accepted`）。run 退役（`forget_run_fences`）时回收。
    unknown_side_effects: std::collections::HashSet<String>,
}

struct WorkflowTaskEntry {
    record: WorkflowTaskRecord,
    /// 执行中取消句柄（消费者经 [`WorkflowTaskTable::mark_executing`] 登记）。
    cancel: Option<CancellationToken>,
    claim_id: Option<String>,
    /// 进入终态的时刻（wall clock 毫秒；仅用于保留窗口淘汰，不序列化）。
    terminal_at_ms: Option<i64>,
}

/// session 级任务表（含任务事件日志、订阅通知与关闭门）。
pub struct WorkflowTaskTable {
    inner: Mutex<TableInner>,
    events: Arc<BoundedEventLog<WorkflowTaskEvent>>,
    updates: broadcast::Sender<WorkflowResourceUpdate>,
    /// 与 [`super::WorkflowRuntime`] 共享的关闭门：关闭后拒绝新发布（对抗审查 F6）。
    gate: Arc<CloseGate>,
    /// 终态通知（装配期一次绑定；见 [`TerminalSink`] 契约）。
    terminal_sink: parking_lot::RwLock<Option<TerminalHandler>>,
}

include!("tasks_section_2.rs");
