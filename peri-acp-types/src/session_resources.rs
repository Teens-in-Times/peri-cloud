//! 会话资源门面 — 消费侧唯一的会话行为契约。
//!
//! 业务侧（Agent / ACP / Controller / TUI）只依赖本 trait 的行为语义，注入的是
//! `Arc<dyn SessionResources>`。数据的持久化位置（本机 SQLite / Turso Cloud）与
//! 本机执行事实（发现、绑定、排空）由资源内部两面分别实现，
//! 数据端口不向业务暴露事务、CAS、SQL batch、连接或补偿令牌。
//!
//! 三条必须成立的约束：
//!
//! 1. **不提供 no-op 默认实现。** 每个行为都是必需方法：无法完成的行为必须返回
//!    明确失败，不能让调用方把「不支持」当成「没有数据」。
//! 2. **声明即保证。** 行为要么满足完整后置条件，要么在副作用前失败；声明
//!    [`DataCapabilities::Complete`] 的 adapter 不得对任何行为退化为 no-op。
//! 3. **效果确定性单独表达。** 失败原因与「是否已生效」分别建模，见
//!    [`SessionResourceError`] 与 [`MutationOutcome`]。
//!
//! 只读入口（`AccessMode::ReadOnly`）不得靠创建一个临时未绑定会话绕过执行授权。
//! 旧 `ThreadStore`（[`crate::store::ThreadStore`]）是迁移桥，见其模块文档。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use crate::history_page::{
    HistoryCursor, HistoryForwardCursor, HistoryPage, HistoryPageBounds, HistorySegment,
};
use crate::history_window::{
    HistoryBodyChunk, HistoryCommittedTail, HistoryReadTicket, HistorySource,
};
use crate::messages::MessageId;
use crate::session::{
    MqCancelOutcome, MqEnqueueOutcome, MqEntry, MqFilter, MqHeader, MqInput, MqReceiveOutcome,
    MqReceiveSelection, MqWatermark,
};
use crate::store::{CompactionChange, MessageFlags, PersistedPayload};
use crate::system_reminder::TrustedSystemReminder;
use crate::thread::{AgentStatus, ThreadId, ThreadMeta, WorkState};
use crate::workspace::{ResolvedWorkspace, ScopedThreadPage, ScopedThreadQuery};

pub mod history;

pub mod history_window;
pub use history_window::{
    HistoryWindowEntry, HistoryWindowPagePlan, HistoryWindowReadIntent, HistoryWindowRows,
    HistoryWindowTailExpectation, InheritedHistoryEntry, SessionHistoryWindowBounds,
};

mod metadata;
pub use metadata::*;

mod error;

pub use error::{
    CloseSettlement, SessionResourceError, SessionResourceErrorKind, SessionResourceResult,
};

/// 未发布创建的句柄（J2 两阶段的第二阶段）：frozen 一次性提交，或整条草稿被撤销。
///
/// 语义（与失败补偿矩阵同源）：
///
/// - 提交是 **write-once CAS**：仅当这条 identity 从未提交过 frozen 时生效；重复调用返回
///   typed 冲突（[`SessionResourceErrorKind::Conflict`]），不覆盖已提交字节；
/// - 提交前检查未决持久化，执行唯一性由 peri-sdk 负责；
/// - [`Self::abandon`] 只撤销**未提交**的草稿（幂等）；已提交 frozen 的草稿必须拒绝删除
///   ——那会销毁一个「已定稿、未发布」的合法中间态；
#[async_trait]
pub trait SessionInitialization: Send + Sync {
    fn thread_id(&self) -> &ThreadId;

    /// 一次性提交 frozen：仅当该 identity 从未提交过；重复调用返回 typed 冲突，不覆盖。
    async fn commit_frozen(&self, frozen: &FrozenSnapshotBytes) -> SessionResourceResult<()>;

    /// 撤销未发布的创建（幂等）；已提交后调用返回 typed 冲突，不删除。
    async fn abandon(self: Arc<Self>) -> SessionResourceResult<()>;
}

/// 子会话 resume 认领 handle。
///
/// 认领期间的 active/原状态/终态由资源内部保存，调用方只提交领域结果，不拼补偿
/// 写入、不接触事务或重试令牌。
#[async_trait]
pub trait ChildResumeClaim: Send + Sync {
    /// 认领成功并开始运行。
    async fn mark_running(&self) -> SessionResourceResult<()>;
    /// 移交后台执行（仍属本次认领）；完成后调用方才可放行后台执行。
    async fn hand_off_to_background(&self) -> SessionResourceResult<()>;
    /// 认领后准备失败：恢复到认领前的状态，不留 active 残留。
    /// 移交失败或移交后启动被取消也可补偿，但调用方须确保后台执行尚未放行。
    async fn mark_failed(&self) -> SessionResourceResult<()>;
    /// 认领终止（取消/宿主退出）：恢复到认领前的状态。
    async fn mark_terminated(&self) -> SessionResourceResult<()>;

    /// 认领终止并提交领域终态，返回本次是否仍拥有该 child 的写入权。
    ///
    /// 实现必须把「恢复到认领前」与终态写入放在同一个归属校验内完成：返回
    /// `Ok(false)` 表示新认领已接管该 child，调用方不得再写终态。
    async fn finish_with_status(&self, status: AgentStatus) -> SessionResourceResult<bool>;
}

mod port;
pub use port::SessionResources;

mod lifecycle;
pub use lifecycle::SessionStoreShutdownPort;

mod settlement;
pub use settlement::TerminalSettlementOutcome;

#[cfg(test)]
#[path = "session_resources_test.rs"]
mod tests;
