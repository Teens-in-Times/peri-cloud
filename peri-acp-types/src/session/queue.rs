//! Queue payload、独立消息策略与有序 Receive 接纳。
//!
//! [`MessageQueue`] 是持久 MQ 的门面：所有事实操作经 [`ThreadMqStore`] 落到存储层，
//! 内存只保留唤醒信号、运行期发送选择与通知版本。破坏性内存 API
//! （`push` / `drain_all` / `drain_batch` / `suppress` / `clear_batch` / `withdraw`）
//! 已移除——待发事实的唯一来源是 MQ 行（`docs/design/thread-mq.md` §4.1）。

use super::mq::{
    MqCancelOutcome, MqEnqueueDisposition, MqEnqueueOutcome, MqEntry, MqFilter, MqHeader, MqInput,
    MqReceiveDisposition, MqReceiveItem, MqReceiveOutcome, MqReceivePurpose, MqReceiveSelection,
    MqReceiveStatus, MqSendItemOutcome, MqSendOutcome, MqSendStatus, MqWatermark,
    SessionResourcesMqStore, ThreadMqStore,
};
use super::{ExecutionBinding, MessageDisposition, MessagePolicy};
use crate::messages::{BaseMessage, MessageId};
use crate::session_resources::SessionResources;
use crate::store::PersistedPayload;
use crate::system_reminder::TrustedSystemReminder;
use crate::thread::ThreadId;
use anyhow::Result;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// 单次 Receive 的上限；与旧 `drain_batch` 的批次上限一致。
const RECEIVE_BATCH_LIMIT: usize = 64;

/// 显式发送等待移交时的查询间隔；只影响确认延迟，不改变任何事实。
const SEND_HANDOFF_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// [`MessageQueue::new`] 的内存 thread 标识；不参与生产路由。
const IN_MEMORY_THREAD_ID: &str = "in-memory";

// ─── MessageKind ─────────────────────────────────────────────────────────────

/// 消息展示标签；运行调度以 MessagePolicy 为准。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// 外部主动请求 — Receive 消费，循环结束后到达同样激活
    Prompt,
    /// 延迟到达的结果 — Receive 消费，循环结束后到达同样激活
    Defer,
    /// 通知性数据 — Receive 消费，永不唤醒循环
    Info,
}

impl MessageKind {
    /// 此展示标签的默认策略；显式策略可覆盖，不能用于运行调度。
    pub fn wakes_up(self) -> bool {
        matches!(self, Self::Prompt | Self::Defer)
    }
}

// ─── MessageSource ───────────────────────────────────────────────────────────

/// 消息来源 — 用于调试和事件追踪
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageSource {
    /// 外部用户输入
    UserInput,
    /// SubAgent 完成
    SubAgentComplete,
    /// 后台 Shell 完成
    ShellComplete,
    /// Goal steering（中途纠正）
    GoalSteering,
    /// Todo steering（requireCompletion 续跑提醒）
    TodoSteering,
    /// Cron 定时触发
    CronTrigger,
    /// Stop hook feedback
    StopHookFeedback,
    /// Channel 消息（微信/Slack 等）
    ChannelMessage,
    /// MCP subscription or lifecycle notification.
    #[serde(alias = "dynamic_mcp_notification")]
    McpNotification,
    /// Hook 系统注入
    SystemInjected,
    /// Hook 的显式停止意图（continue:false）：经 Receive 唯一出口停止当前 run，
    /// 不按可唤醒消息处理，也不发起额外模型请求。
    HookStopIntent,
    /// 工具失败警告
    ToolFailureWarning,
    /// 工作流完成
    WorkflowComplete,
    /// 进入会话时的恢复提醒（进程中断后从已提交历史继续）。
    ///
    /// 只有 Host 恢复协调以可信 SystemReminder 构造；旧行没有该来源，反序列化不受影响。
    SessionRecovery,
}

// ─── QueuedMessage ───────────────────────────────────────────────────────────

/// Queue payload. Scheduling (`MessageKind`) is deliberately orthogonal to content semantics.
#[derive(Debug, Clone)]
pub enum QueuedPayload {
    Message(BaseMessage),
    SystemReminder(TrustedSystemReminder),
}

/// 一条待投递的消息（v2 富类型）
#[derive(Debug, Clone)]
pub struct QueuedMessage {
    /// 消息 Kind（决定默认策略）
    pub kind: MessageKind,
    /// 消息来源
    pub source: MessageSource,
    /// 实际消息内容
    pub payload: QueuedPayload,
    /// Stable canonical ID for an owner-confirmed terminal reminder.
    pub delivery_id: Option<MessageId>,
    pub policy: MessagePolicy,
    /// 旧内存队列的接纳序号；MQ 用 `(enqueued_at, id)` 定序，本字段不再参与排序。
    pub admission_sequence: Option<u64>,
    /// 用户输入的原始编辑器文本；其他来源为 `None`。
    pub original_draft: Option<String>,
    /// 存储生成的入队时间；只有从 MQ 读回的条目有值。
    pub enqueued_at: Option<String>,
}

impl QueuedMessage {
    pub fn new(kind: MessageKind, source: MessageSource, message: BaseMessage) -> Self {
        Self::with_payload(kind, source, QueuedPayload::Message(message))
    }

    pub fn with_payload(kind: MessageKind, source: MessageSource, payload: QueuedPayload) -> Self {
        let mut policy = match kind {
            MessageKind::Prompt | MessageKind::Defer => MessagePolicy::ensure_processing(),
            MessageKind::Info => MessagePolicy::passive(),
        };
        if let QueuedPayload::SystemReminder(reminder) = &payload {
            policy.model_visible = reminder
                .as_reminder()
                .audiences
                .0
                .contains(&crate::system_reminder::ReminderAudience::Model);
        }
        Self {
            kind,
            source,
            payload,
            delivery_id: None,
            policy,
            admission_sequence: None,
            original_draft: None,
            enqueued_at: None,
        }
    }

    pub fn with_policy(mut self, mut policy: MessagePolicy) -> Self {
        if let QueuedPayload::SystemReminder(reminder) = &self.payload {
            policy.model_visible &= reminder
                .as_reminder()
                .audiences
                .0
                .contains(&crate::system_reminder::ReminderAudience::Model);
        }
        self.policy = policy;
        self
    }

    /// 附带用户输入的原始编辑器文本（只有待发送区的用户输入需要）。
    pub fn with_original_draft(mut self, draft: impl Into<String>) -> Self {
        self.original_draft = Some(draft.into());
        self
    }

    /// 附带入队时间；仅用于从 MQ 行构造视图，入队请求不接受调用方指定时间。
    pub fn with_enqueued_at(mut self, enqueued_at: impl Into<String>) -> Self {
        self.enqueued_at = Some(enqueued_at.into());
        self
    }

    pub fn system_reminder_with_delivery_id(
        kind: MessageKind,
        source: MessageSource,
        reminder: TrustedSystemReminder,
        delivery_id: MessageId,
    ) -> Self {
        let mut message = Self::system_reminder(kind, source, reminder);
        message.delivery_id = Some(delivery_id);
        message
    }

    pub fn system_reminder(
        kind: MessageKind,
        source: MessageSource,
        reminder: TrustedSystemReminder,
    ) -> Self {
        Self::with_payload(kind, source, QueuedPayload::SystemReminder(reminder))
    }

    pub fn message(&self) -> Option<&BaseMessage> {
        match &self.payload {
            QueuedPayload::Message(message) => Some(message),
            QueuedPayload::SystemReminder(_) => None,
        }
    }

    /// 快速构造 Prompt 消息（用户输入）
    pub fn prompt(source: MessageSource, message: BaseMessage) -> Self {
        Self::new(MessageKind::Prompt, source, message)
    }

    /// 快速构造 Defer 消息（SubAgent/Cron/Channel/Workflow 延迟结果）
    pub fn defer(source: MessageSource, message: BaseMessage) -> Self {
        Self::new(MessageKind::Defer, source, message)
    }

    /// 快速构造 Info 消息（SystemReminder/Hook 注入，不唤醒循环）
    pub fn info(source: MessageSource, message: BaseMessage) -> Self {
        Self::new(MessageKind::Info, source, message)
    }

    /// 稳定投递身份：`Message` 用载荷 id，`SystemReminder` 用已固定的 `delivery_id`。
    ///
    /// 未固定的提醒每次调用都会得到新 id——需要稳定身份时用 [`MessageQueue::enqueue`]
    /// 返回的 `outcome.entry.id`（入队时只固定一次）。
    pub fn id(&self) -> MessageId {
        match &self.payload {
            QueuedPayload::Message(message) => message.id(),
            QueuedPayload::SystemReminder(_) => self.delivery_id.unwrap_or_default(),
        }
    }

    /// 转为入队请求；`SystemReminder` 的 `delivery_id` 在这里固定一次，行 id 与载荷 id 一致。
    pub fn into_mq_input(mut self) -> MqInput {
        let payload = match self.payload {
            QueuedPayload::Message(message) => PersistedPayload::Message(message),
            QueuedPayload::SystemReminder(reminder) => {
                let id = self.delivery_id.unwrap_or_default();
                self.delivery_id = Some(id);
                PersistedPayload::SystemReminder { id, reminder }
            }
        };
        MqInput {
            id: payload.id(),
            kind: self.kind,
            source: self.source,
            payload,
            policy: self.policy,
            original_draft: self.original_draft,
        }
    }

    /// MQ 行 → 队列视图：策略以存储行为准，不按 kind 重算。
    pub fn from_mq_entry(entry: MqEntry) -> Self {
        let MqEntry {
            id: _,
            enqueued_at,
            kind,
            source,
            payload,
            policy,
            original_draft,
        } = entry;
        let (payload, delivery_id) = match payload {
            PersistedPayload::Message(message) => (QueuedPayload::Message(message), None),
            PersistedPayload::SystemReminder { id, reminder } => {
                (QueuedPayload::SystemReminder(reminder), Some(id))
            }
        };
        Self {
            kind,
            source,
            payload,
            delivery_id,
            policy,
            admission_sequence: None,
            original_draft,
            enqueued_at: Some(enqueued_at),
        }
    }
}

// ─── MessageQueue ────────────────────────────────────────────────────────────

/// 关闭结算开始后拒绝入队；携带 thread 便于调用方诊断。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueClosed {
    pub thread_id: String,
}

impl std::fmt::Display for QueueClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "message queue for thread {} is closed to new enqueues",
            self.thread_id
        )
    }
}

impl std::error::Error for QueueClosed {}

/// 会话级持久收件箱门面（绑定一个 thread）。
///
/// 待发事实（有哪些消息、顺序、策略、移交与取消）全部由 [`ThreadMqStore`] 回答；
/// 本类型在内存里只保留唤醒信号、运行期发送选择与通知版本。唤醒只提示重查，
/// 不能当作消息存在性或消费资格的依据。
///
/// 生产必须用 [`bind`](Self::bind)；[`new`](Self::new) 使用内存后端，仅供测试与无持久化
/// 的嵌入场景。
#[derive(Clone)]
pub struct MessageQueue {
    inner: Arc<Mailbox>,
}

struct Mailbox {
    store: Arc<dyn ThreadMqStore>,
    thread_id: ThreadId,
    wake: tokio::sync::Notify,
    wake_version: tokio::sync::watch::Sender<u64>,
    send_selection: Mutex<Vec<MessageId>>,
    /// 已受理但尚未提交的入队数量（同步 hook 桥与直接入队共用）。
    pending_enqueues: AtomicUsize,
    /// 入队闸门：关闭结算开始后拒绝新入队；clone 共享。
    ///
    /// 只存在于本门面，不写入存储：同一 thread 的新 [`bind`](MessageQueue::bind)
    /// （新 owner / resume）是新的开放门面。
    admission_closed: AtomicBool,
    pending_drained: tokio::sync::Notify,
    /// 仅测试：记录被 `suppress_sync` 处置的消息，供旧断言读取。
    #[cfg(feature = "test-support")]
    suppressed: Mutex<Vec<QueuedMessage>>,
}

/// 一次分离入队的在途凭证；Drop 即宣告该写入结束（见 [`MessageQueue::begin_detached_enqueue`]）。
pub struct DetachedEnqueue {
    inner: Arc<Mailbox>,
}

impl Drop for DetachedEnqueue {
    fn drop(&mut self) {
        if self.inner.pending_enqueues.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.inner.pending_drained.notify_waiters();
        }
    }
}

impl Default for MessageQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for MessageQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageQueue")
            .field("thread_id", &self.inner.thread_id)
            .field("wake_version", &*self.inner.wake_version.borrow())
            .field("send_selection", &self.inner.send_selection.lock().len())
            .finish()
    }
}

impl MessageQueue {
    /// 仅测试/嵌入：内存 MQ 后端 + 固定 thread（见 [`bind`](Self::bind)）。
    pub fn new() -> Self {
        Self::from_store(
            Arc::new(super::mq_memory::InMemoryMqStore::new()),
            IN_MEMORY_THREAD_ID.to_owned(),
        )
    }

    /// 生产入口：把门面绑定到真实资源与 thread。
    pub fn bind(resources: Arc<dyn SessionResources>, thread_id: ThreadId) -> Self {
        Self::from_store(Arc::new(SessionResourcesMqStore::new(resources)), thread_id)
    }

    fn from_store(store: Arc<dyn ThreadMqStore>, thread_id: ThreadId) -> Self {
        Self {
            inner: Arc::new(Mailbox {
                store,
                thread_id,
                wake: tokio::sync::Notify::new(),
                wake_version: tokio::sync::watch::channel(0).0,
                send_selection: Mutex::new(Vec::new()),
                pending_enqueues: AtomicUsize::new(0),
                admission_closed: AtomicBool::new(false),
                pending_drained: tokio::sync::Notify::new(),
                #[cfg(feature = "test-support")]
                suppressed: Mutex::new(Vec::new()),
            }),
        }
    }

    /// 本门面绑定的 thread。
    pub fn thread_id(&self) -> &ThreadId {
        &self.inner.thread_id
    }

    /// 登记一次「已受理、尚未提交」的分离入队。
    ///
    /// 同步 middleware hook 无法就地 await 持久提交，只能把写入投递到 runtime；
    /// 调用方在下一次 [`Self::await_pending_enqueues`] 之前不得据此断定收件箱为空。
    /// 返回的 guard 在写入结束（成功或失败）时 Drop，才把入队计为已完成。
    pub fn begin_detached_enqueue(&self) -> DetachedEnqueue {
        self.inner.pending_enqueues.fetch_add(1, Ordering::SeqCst);
        DetachedEnqueue {
            inner: Arc::clone(&self.inner),
        }
    }

    /// 关闭入队准入；幂等。
    ///
    /// 关闭结算在读取 MQ 快照前调用，随后 [`Self::await_pending_enqueues`] 等到
    /// 已通过闸门的写入结束，结算看到的行集合才稳定。结算自身的存储写入
    /// （`settle_retained_mq`）直接经资源门面，不经过本闸门。
    pub fn close_admission(&self) {
        self.inner.admission_closed.store(true, Ordering::SeqCst);
    }

    /// 入队闸门是否已关闭（clone 共享）。
    pub fn is_admission_closed(&self) -> bool {
        self.inner.admission_closed.load(Ordering::SeqCst)
    }

    /// 为一次外部入队登记在途凭证，并检查关闭闸门。
    ///
    /// 先登记计数再读闸门：close 先关闸、再等计数归零，因此不会漏掉
    /// 「已通过检查但尚未提交」的写入。被拒绝的调用不写存储，返回
    /// [`QueueClosed`]。
    fn admit_enqueue(&self, message: &QueuedMessage) -> Result<DetachedEnqueue> {
        let pending = self.begin_detached_enqueue();
        if self.is_admission_closed() {
            tracing::warn!(
                thread_id = %self.inner.thread_id,
                kind = ?message.kind,
                source = ?message.source,
                requirement = ?message.policy.requirement,
                "message queue admission is closed; enqueue rejected"
            );
            return Err(anyhow::Error::new(QueueClosed {
                thread_id: self.inner.thread_id.to_string(),
            }));
        }
        Ok(pending)
    }

    /// 等待所有已受理的分离入队提交完成。
    ///
    /// Receive 在选择前缀前必须调用：否则本轮选择会漏掉刚由 hook 投递的消息，
    /// 循环据此判定空闲并退出。
    pub async fn await_pending_enqueues(&self) {
        loop {
            let notified = self.inner.pending_drained.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.inner.pending_enqueues.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.await;
        }
    }

    // ── 事实操作 ──

    /// 入队一条消息并返回接纳结论；只有可唤醒策略（非 Passive）才发布唤醒。
    ///
    /// 调用方需要稳定 id 时用返回的 `outcome.entry.id`：未固定的 `SystemReminder`
    /// `delivery_id` 在本次入队时生成，重试必须复用它。关闭结算开始后返回
    /// [`QueueClosed`]，不写存储。
    pub async fn enqueue(&self, message: QueuedMessage) -> Result<MqEnqueueOutcome> {
        let notifies = message.policy.notifies_execution();
        let _pending = self.admit_enqueue(&message)?;
        let outcome = self
            .inner
            .store
            .mq_enqueue(&self.inner.thread_id, message.into_mq_input())
            .await?;
        if notifies && outcome.disposition != MqEnqueueDisposition::AlreadyHandedOff {
            self.notify_wake();
        }
        Ok(outcome)
    }

    /// 入队并立即放行：非交互入口没有待发送区，用户输入必须在同一步进入发送选择。
    ///
    /// 用户待发输入走 mailbox `enqueue` + `dispatch`，不能使用本方法绕过准入。
    pub async fn enqueue_sent(&self, message: QueuedMessage) -> Result<MqEnqueueOutcome> {
        let id = message.id();
        let outcome = self.enqueue(message).await?;
        if outcome.disposition != MqEnqueueDisposition::AlreadyHandedOff {
            // 这里只写入发送选择：启动路径随后才跑 Receive，不能在此等待移交。
            self.accept_for_send(vec![id]).await?;
        }
        Ok(outcome)
    }

    /// 同步能力面的入队：仅当存储声明无需 I/O 时才就地完成，否则返回 `None`。
    ///
    /// 同步 hook 无法 await；内存存储可以立即提交，调用方据此保持"返回即已存在"
    /// 的可见性，真实存储仍由调用方投递到 runtime。
    #[doc(hidden)]
    pub fn enqueue_now(&self, message: QueuedMessage) -> Option<Result<MqEnqueueOutcome>> {
        self.inner
            .store
            .supports_immediate_enqueue()
            .then(|| futures::executor::block_on(self.enqueue(message)))
    }

    /// 批量入队（逐条提交）；任一失败立即返回，调用方按稳定 ID 重查已提交的部分。
    ///
    /// 关闭结算开始后拒绝后续条目，已提交前缀保持持久事实。
    pub async fn enqueue_batch(
        &self,
        messages: Vec<QueuedMessage>,
    ) -> Result<Vec<MqEnqueueOutcome>> {
        let mut outcomes = Vec::with_capacity(messages.len());
        let mut wake = false;
        for message in messages {
            let notifies = message.policy.notifies_execution();
            let _pending = self.admit_enqueue(&message)?;
            let outcome = self
                .inner
                .store
                .mq_enqueue(&self.inner.thread_id, message.into_mq_input())
                .await?;
            if notifies && outcome.disposition != MqEnqueueDisposition::AlreadyHandedOff {
                wake = true;
            }
            outcomes.push(outcome);
        }
        if wake {
            self.notify_wake();
        }
        Ok(outcomes)
    }

    /// 按投影用途读取本 thread 的 MQ 行；只读，不消费。
    pub async fn query(&self, filter: MqFilter) -> Result<Vec<MqEntry>> {
        Ok(self
            .inner
            .store
            .mq_query(&self.inner.thread_id, filter)
            .await?)
    }

    /// 全部 MQ 行转成队列视图（非破坏性冻结引用）。
    pub async fn snapshot(&self) -> Result<Vec<QueuedMessage>> {
        Ok(self
            .query(MqFilter::default())
            .await?
            .into_iter()
            .map(QueuedMessage::from_mq_entry)
            .collect())
    }

    /// 取消一条仍在 MQ 的用户输入，返回整行原稿。
    pub async fn cancel_user_input(&self, id: &MessageId) -> Result<MqCancelOutcome> {
        Ok(self
            .inner
            .store
            .mq_cancel(&self.inner.thread_id, *id)
            .await?)
    }

    /// 提交运行期发送选择：只接受仍在 MQ 的用户输入，不写历史（`§5.3`）。
    ///
    /// 只报告当下事实：被接纳的消息仍是待发事实（`Accepted`），直到 Receive 移交成功。
    /// 未命中的 id 按历史的持久事实区分「已在历史」与「不存在」，无法查询时报告 `Unknown`。
    /// 需要确认移交时调用 [`Self::send_inputs_within`]（有界等待，不得在阻塞运行时里用）。
    pub async fn send_inputs(&self, ids: Vec<MessageId>) -> Result<MqSendOutcome> {
        self.accept_for_send(ids).await
    }

    /// [`Self::send_inputs`] 的有界等待版本：尽可能确认移交，超时仍报 `Accepted`。
    ///
    /// 等待必须有界（`handoff_timeout` 由调用方给出）；必须从活跃的异步上下文调用，
    /// 调用方若在 `block_on` 里等待它，就会阻塞唯一能推进 Receive 的线程。
    pub async fn send_inputs_within(
        &self,
        ids: Vec<MessageId>,
        handoff_timeout: std::time::Duration,
    ) -> Result<MqSendOutcome> {
        let mut outcome = self.accept_for_send(ids).await?;
        // 没有 Tokio 运行时就没有并发的 Receive：不进入周期性查询（也无法 sleep），
        // 如实报告「已接纳未移交」。
        let handoff_timeout = if tokio::runtime::Handle::try_current().is_err() {
            std::time::Duration::ZERO
        } else {
            handoff_timeout
        };
        if handoff_timeout.is_zero() {
            return Ok(outcome);
        }
        let deadline = std::time::Instant::now() + handoff_timeout;
        // 只等待仍记作 Accepted 的项：其余结论已经由存储事实给出。
        loop {
            let pending = outcome
                .items
                .iter()
                .filter(|item| item.status == MqSendStatus::Accepted)
                .map(|item| item.id)
                .collect::<Vec<_>>();
            if pending.is_empty() {
                return Ok(outcome);
            }
            let entries = match self.query(MqFilter::default()).await {
                Ok(entries) => entries,
                Err(error) => {
                    // 发送选择已经提交：查询失败只能停止确认，不能把整次调用变成
                    // 「未接纳」，否则调用方会误以为重试是安全的。
                    tracing::warn!(
                        %error,
                        "hand-off confirmation query failed; keeping accepted status"
                    );
                    return Ok(outcome);
                }
            };
            for item in &mut outcome.items {
                if item.status != MqSendStatus::Accepted || entries.iter().any(|e| e.id == item.id)
                {
                    continue;
                }
                item.status = match self
                    .inner
                    .store
                    .mq_history_contains(&self.inner.thread_id, item.id)
                    .await
                {
                    Ok(true) => MqSendStatus::HandedOff,
                    Ok(false) => MqSendStatus::NotFound,
                    Err(error) => {
                        tracing::warn!(%error, "hand-off confirmation failed; reporting unknown");
                        MqSendStatus::Unknown
                    }
                };
                if item.status != MqSendStatus::HandedOff {
                    item.entry = None;
                }
            }
            if std::time::Instant::now() >= deadline {
                return Ok(outcome);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            tokio::time::sleep(remaining.min(SEND_HANDOFF_POLL_INTERVAL)).await;
        }
    }

    /// 只写入发送选择并报告当下事实，不做移交等待（`enqueue_sent` 的同步语义）。
    async fn accept_for_send(&self, ids: Vec<MessageId>) -> Result<MqSendOutcome> {
        let entries = self
            .query(MqFilter {
                user_input_only: true,
                ..Default::default()
            })
            .await?;
        let mut items = Vec::with_capacity(ids.len());
        let mut accepted_ids = Vec::new();
        for id in ids {
            match entries.iter().find(|entry| entry.id == id) {
                Some(entry) => {
                    if !accepted_ids.contains(&id) {
                        accepted_ids.push(id);
                    }
                    items.push(MqSendItemOutcome {
                        id,
                        status: MqSendStatus::Accepted,
                        entry: Some(entry.clone()),
                    });
                }
                None => {
                    // 先查持久历史事实，再一次性写入发送选择：锁不能跨 await。
                    let status = match self
                        .inner
                        .store
                        .mq_history_contains(&self.inner.thread_id, id)
                        .await
                    {
                        Ok(true) => MqSendStatus::AlreadyInHistory,
                        Ok(false) => MqSendStatus::NotFound,
                        Err(error) => {
                            tracing::warn!(
                                %error,
                                "history lookup failed for a missing send id; reporting unknown"
                            );
                            MqSendStatus::Unknown
                        }
                    };
                    items.push(MqSendItemOutcome {
                        id,
                        status,
                        entry: None,
                    });
                }
            }
        }
        if !accepted_ids.is_empty() {
            let mut selection = self.inner.send_selection.lock();
            for id in accepted_ids {
                if !selection.contains(&id) {
                    selection.push(id);
                }
            }
            drop(selection);
            self.notify_wake();
        }
        Ok(MqSendOutcome { items })
    }

    /// 取走本次运行期发送选择（仅内存，不影响 MQ 行）。
    pub fn take_send_selection(&self) -> Vec<MessageId> {
        std::mem::take(&mut *self.inner.send_selection.lock())
    }

    /// 撤回指定的运行期发送选择；MQ 行保持不变。
    ///
    /// Stop/失败收尾恢复 `Queued` 时使用：选择已被取消，旧执行不能借此消费。
    pub fn withdraw_send_inputs(&self, ids: &[MessageId]) {
        self.inner
            .send_selection
            .lock()
            .retain(|id| !ids.contains(id));
    }
}

include!("queue_consumer.rs");

include!("queue_capabilities.rs");

#[cfg(test)]
#[path = "queue_mq_test.rs"]
mod mq_tests;

#[cfg(test)]
#[path = "queue_source_test.rs"]
mod source_tests;

#[cfg(test)]
#[path = "queue_wake_test.rs"]
mod wake_tests;
