// ─── 能力视图：生产者 / 生命周期 owner ────────────────────────────────────────
//
// 同一份 MQ 事实有两个能力面：生产者只能入队、查询与写发送选择；Receive 与清空
// 会改变消费事实，只出现在生命周期 owner 视图上。两者是不同名义类型，生产者拿到的
// （`InboxHandle::queue`）没有通往 owner 视图的方法，误用在编译期就失败。

/// 生产者视图：入队、只读查询与运行期发送选择，不含消费与清空。
#[derive(Clone, Copy, Debug)]
pub struct ProducerQueue<'a> {
    queue: &'a MessageQueue,
}

/// 生命周期 owner 视图：Receive 许可、原子移交与 thread 级清空。
///
/// 由 [`MessageQueue::owner`] 取得；调用方必须是 Agent Loop、rewind 或 thread 关闭
/// 等生命周期 owner。
#[derive(Clone, Copy, Debug)]
pub struct QueueOwner<'a> {
    queue: &'a MessageQueue,
}

impl MessageQueue {
    /// 生产者能力面（窄接口）。
    pub fn producer(&self) -> ProducerQueue<'_> {
        ProducerQueue { queue: self }
    }

    /// 生命周期 owner 能力面（含消费与清空）。
    pub fn owner(&self) -> QueueOwner<'_> {
        QueueOwner { queue: self }
    }
}

impl ProducerQueue<'_> {
    /// 同一 mailbox 的底层门面；只在 crate 内用于构造可 `'static` 的窄句柄。
    pub(crate) fn queue_ref(&self) -> &MessageQueue {
        self.queue
    }

    pub fn thread_id(&self) -> &ThreadId {
        self.queue.thread_id()
    }

    pub fn begin_detached_enqueue(&self) -> DetachedEnqueue {
        self.queue.begin_detached_enqueue()
    }

    pub async fn await_pending_enqueues(&self) {
        self.queue.await_pending_enqueues().await;
    }

    pub async fn enqueue(&self, message: QueuedMessage) -> Result<MqEnqueueOutcome> {
        self.queue.enqueue(message).await
    }

    pub async fn enqueue_sent(&self, message: QueuedMessage) -> Result<MqEnqueueOutcome> {
        self.queue.enqueue_sent(message).await
    }

    pub fn enqueue_now(&self, message: QueuedMessage) -> Option<Result<MqEnqueueOutcome>> {
        self.queue.enqueue_now(message)
    }

    pub async fn enqueue_batch(
        &self,
        messages: Vec<QueuedMessage>,
    ) -> Result<Vec<MqEnqueueOutcome>> {
        self.queue.enqueue_batch(messages).await
    }

    pub async fn query(&self, filter: MqFilter) -> Result<Vec<MqEntry>> {
        self.queue.query(filter).await
    }

    pub async fn snapshot(&self) -> Result<Vec<QueuedMessage>> {
        self.queue.snapshot().await
    }

    pub async fn cancel_user_input(&self, id: &MessageId) -> Result<MqCancelOutcome> {
        self.queue.cancel_user_input(id).await
    }

    pub async fn send_inputs(&self, ids: Vec<MessageId>) -> Result<MqSendOutcome> {
        self.queue.send_inputs(ids).await
    }

    pub async fn send_inputs_within(
        &self,
        ids: Vec<MessageId>,
        timeout: Duration,
    ) -> Result<MqSendOutcome> {
        self.queue.send_inputs_within(ids, timeout).await
    }

    pub fn take_send_selection(&self) -> Vec<MessageId> {
        self.queue.take_send_selection()
    }

    pub fn withdraw_send_inputs(&self, ids: &[MessageId]) {
        self.queue.withdraw_send_inputs(ids);
    }

    pub async fn await_wake(&self) {
        self.queue.await_wake().await;
    }

    pub fn subscribe_wake(&self) -> tokio::sync::watch::Receiver<u64> {
        self.queue.subscribe_wake()
    }

    pub async fn has_ensure_processing(&self) -> Result<bool> {
        self.queue.has_ensure_processing().await
    }

    pub async fn has_required(&self) -> Result<bool> {
        self.queue.has_required().await
    }

    pub async fn has_wake_up(&self) -> Result<bool> {
        self.queue.has_wake_up().await
    }

    pub async fn is_empty(&self) -> Result<bool> {
        self.queue.is_empty().await
    }

    pub async fn len(&self) -> Result<usize> {
        self.queue.len().await
    }
}

impl QueueOwner<'_> {
    /// 取一次 Receive 许可；许可不可 Clone，移交时被消费。
    pub fn begin_receive(
        &self,
        purpose: MqReceivePurpose,
        execution: ExecutionBinding,
    ) -> ReceivePermit {
        self.queue.begin_receive(purpose, execution)
    }

    pub async fn select_for_receive(&self, permit: &ReceivePermit) -> Result<Vec<MqReceiveItem>> {
        self.queue.select_for_receive(permit).await
    }

    pub async fn receive(
        &self,
        permit: ReceivePermit,
        items: Vec<MqReceiveItem>,
    ) -> Result<MqReceiveOutcome> {
        self.queue.receive(permit, items).await
    }

    /// 清空本 thread 的全部 MQ 行：只给 rewind 与 thread 生命周期调用。
    pub async fn clear(&self) -> Result<()> {
        self.queue.clear().await
    }
}

// ─── ReceivePermit ───────────────────────────────────────────────────────────

/// 行是否可能被本轮 Receive 取用。
///
/// 未发送的用户输入留在待发送区：即使策略要求处理，也要 `send_inputs` 放行后
/// 才参与本轮消费。所有「是否还有本轮工作」的判定都必须与 Receive 的选择一致，
/// 否则会出现「判定有工作但取不出」的空转。
fn is_eligible_for_receive(entry: &MqEntry, selection: &[MessageId]) -> bool {
    source_is_eligible(&entry.source, &entry.id, selection)
}

/// 控制面 header 与完整行共用同一条 eligibility 规则。
fn is_header_eligible(header: &MqHeader, selection: &[MessageId]) -> bool {
    source_is_eligible(&header.source, &header.id, selection)
}

fn source_is_eligible(source: &MessageSource, id: &MessageId, selection: &[MessageId]) -> bool {
    *source != MessageSource::UserInput || selection.contains(id)
}

/// 空内容 Prompt 是 keepgoing 等控制信号的唤醒载体，消费后不得写入历史。
fn is_wake_only(entry: &MqEntry) -> bool {
    entry.kind == MessageKind::Prompt
        && matches!(
            &entry.payload,
            PersistedPayload::Message(BaseMessage::Human { content, .. }) if content.is_empty()
        )
}

/// 一次 Receive 的运行期许可。
///
/// 字段私有、不实现 `Clone`：只有 Agent Loop 或同 thread 生命周期 owner 能经
/// [`MessageQueue::begin_receive`] 取得，[`MessageQueue::receive`] 消费它。
#[derive(Debug)]
pub struct ReceivePermit {
    execution: ExecutionBinding,
    purpose: MqReceivePurpose,
}

impl ReceivePermit {
    fn into_selection(self, items: Vec<MqReceiveItem>) -> MqReceiveSelection {
        MqReceiveSelection {
            items,
            purpose: self.purpose,
            execution: self.execution,
        }
    }
}

// ─── 测试同步适配（仅 feature = "test-support"） ─────────────────────────────
//
// 旧单元测试以同步断言描述队列行为。这里用同一个持久门面，不复制事实源、不引入
// 第二份内存权威；生产构建不包含这些方法。
//
// 同步桥在 tokio 运行时内只在首次 poll 即可完成时返回：阻塞 runtime 线程会让同
// runtime 上持有写入屏障的任务永远得不到调度，从挂起变成死锁；这类调用必须改用
// 异步 API。确需同步等待的回调（如 mock server）应换独立 runtime 线程执行。

#[cfg(feature = "test-support")]
fn ready_only<T>(future: impl std::future::Future<Output = T>) -> T {
    if tokio::runtime::Handle::try_current().is_err() {
        return futures::executor::block_on(future);
    }
    use futures::FutureExt;
    let mut future = Box::pin(future);
    match future.as_mut().now_or_never() {
        Some(value) => value,
        None => panic!(
            "test MQ sync API cannot await inside a tokio runtime; use the async API instead"
        ),
    }
}

#[cfg(feature = "test-support")]
impl MessageQueue {
    fn test_permit(&self) -> ReceivePermit {
        self.begin_receive(
            MqReceivePurpose::NonModelSettlement,
            super::ExecutionBinding {
                turn_id: super::TurnId::new(),
                attempt_id: crate::identity::AttemptId::new(),
            },
        )
    }

    /// 仅测试：下一次原子移交在写入前暂停；返回（进入通知，放行信号）。
    pub fn gate_next_receive(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        self.inner
            .store
            .install_receive_gate((entered_tx, release_rx));
        (entered_rx, release_tx)
    }

    /// 仅测试：下一次 MQ 查询在读取前暂停；返回（进入通知，放行信号）。
    pub fn gate_next_query(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        self.inner
            .store
            .install_query_gate((entered_tx, release_rx));
        (entered_rx, release_tx)
    }

    pub fn push_sync(&self, message: QueuedMessage) {
        ready_only(self.push_async(message));
    }

    pub fn push_batch_sync(&self, messages: Vec<QueuedMessage>) {
        ready_only(self.push_batch_async(messages));
    }

    /// 仅测试：`push_sync` 的异步等价，可在 tokio 运行时内安全 await。
    pub async fn push_async(&self, message: QueuedMessage) {
        let id = message.id();
        let is_user_input = message.source == MessageSource::UserInput;
        self.enqueue(message).await.expect("test MQ enqueue");
        if is_user_input {
            // 旧测试的 `push` 语义是“立即可被 Receive 消费”；显式写入选择，
            // 与 `send_inputs` 的筛选保持一致。
            let mut selection = self.inner.send_selection.lock();
            if !selection.contains(&id) {
                selection.push(id);
            }
        }
    }

    pub async fn push_batch_async(&self, messages: Vec<QueuedMessage>) {
        for message in messages {
            self.push_async(message).await;
        }
    }

    pub fn enqueue_batch_sync(&self, messages: Vec<QueuedMessage>) -> Vec<MqEnqueueOutcome> {
        messages
            .into_iter()
            .map(|message| ready_only(self.enqueue(message)).expect("test MQ enqueue"))
            .collect()
    }

    pub fn drain_all_sync(&self) -> Vec<QueuedMessage> {
        ready_only(self.drain_all_async())
    }

    pub fn drain_batch_sync(&self, limit: usize) -> Vec<QueuedMessage> {
        ready_only(self.drain_batch_async(limit))
    }

    pub async fn drain_all_async(&self) -> Vec<QueuedMessage> {
        self.drain_batch_async(usize::MAX).await
    }

    /// 仅测试：`drain_batch_sync` 的异步等价，可在 tokio 运行时内安全 await。
    pub async fn drain_batch_async(&self, limit: usize) -> Vec<QueuedMessage> {
        let selection = self.inner.send_selection.lock().clone();
        let entries = self
            .query(MqFilter::default())
            .await
            .expect("test MQ query");
        let items = entries
            .into_iter()
            .filter(|entry| {
                entry.source != MessageSource::UserInput || selection.contains(&entry.id)
            })
            .take(limit)
            .map(|entry| MqReceiveItem {
                entry,
                disposition: MqReceiveDisposition::Commit,
            })
            .collect();
        match self.receive(self.test_permit(), items).await {
            Ok(outcome) => outcome
                .items
                .into_iter()
                .filter_map(|item| item.entry.map(QueuedMessage::from_mq_entry))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn suppress_sync(&self, message: QueuedMessage) {
        let id = message.id();
        let entries = ready_only(self.query(MqFilter::default())).expect("test MQ query");
        let Some(entry) = entries.into_iter().find(|entry| entry.id == id) else {
            return;
        };
        let _ = ready_only(self.receive(
            self.test_permit(),
            vec![MqReceiveItem {
                entry,
                disposition: MqReceiveDisposition::Suppress,
            }],
        ));
    }

    pub fn suppressed_messages(&self) -> Vec<QueuedMessage> {
        self.inner.suppressed.lock().clone()
    }

    pub fn withdraw_user_inputs_sync(&self, ids: &[MessageId]) -> Vec<QueuedMessage> {
        ids.iter()
            .filter_map(|id| match ready_only(self.cancel_user_input(id)) {
                Ok(MqCancelOutcome::Cancelled(entry)) => Some(QueuedMessage::from_mq_entry(entry)),
                _ => None,
            })
            .collect()
    }

    pub fn snapshot_sync(&self) -> Vec<QueuedMessage> {
        ready_only(self.snapshot()).expect("test MQ snapshot")
    }

    pub fn clear_sync(&self) {
        ready_only(self.clear()).expect("test MQ clear");
    }

    pub fn len_sync(&self) -> usize {
        let selection = self.inner.send_selection.lock().clone();
        let entries = ready_only(self.query(MqFilter::default())).expect("test MQ len");
        entries
            .iter()
            .filter(|entry| {
                entry.source != MessageSource::UserInput || selection.contains(&entry.id)
            })
            .count()
    }

    pub fn is_empty_sync(&self) -> bool {
        self.len_sync() == 0
    }

    pub fn has_wake_up_sync(&self) -> bool {
        ready_only(self.has_wake_up()).expect("test MQ wake")
    }

    pub fn has_ensure_processing_sync(&self) -> bool {
        ready_only(self.has_ensure_processing()).expect("test MQ policy")
    }

    pub fn has_required_sync(&self) -> bool {
        ready_only(self.has_required()).expect("test MQ requirement")
    }

    pub fn has_required_for_run_sync(&self, execution: &ExecutionBinding) -> bool {
        ready_only(self.has_required_for_run(execution)).expect("test MQ run requirement")
    }

    pub fn has_pending_defer_sync(&self, source: &MessageSource) -> bool {
        ready_only(self.has_pending_defer(source)).expect("test MQ defer")
    }

    pub fn needs_mq_continuation_sync(&self) -> bool {
        ready_only(self.needs_mq_continuation()).expect("test MQ continuation")
    }

    pub fn admission_watermark_sync(&self) -> Option<MqWatermark> {
        ready_only(self.admission_watermark()).expect("test MQ watermark")
    }

    pub fn has_ensure_processing_after_sync(&self, watermark: &Option<MqWatermark>) -> bool {
        ready_only(self.has_ensure_processing_after(watermark)).expect("test MQ watermark policy")
    }
}

#[cfg(feature = "test-support")]
impl ProducerQueue<'_> {
    pub fn push_sync(&self, message: QueuedMessage) {
        self.queue.push_sync(message);
    }

    pub fn push_batch_sync(&self, messages: Vec<QueuedMessage>) {
        self.queue.push_batch_sync(messages);
    }
}

#[cfg(feature = "test-support")]
impl super::InboxHandle {
    pub fn push_sync(&self, message: QueuedMessage) {
        self.queue().push_sync(message);
    }

    pub fn push_prompt_sync(&self, source: MessageSource, message: BaseMessage) {
        self.push_sync(QueuedMessage::prompt(source, message));
    }

    pub fn push_defer_sync(&self, source: MessageSource, message: BaseMessage) {
        self.push_sync(QueuedMessage::defer(source, message));
    }

    pub fn push_info_sync(&self, source: MessageSource, message: BaseMessage) {
        self.push_sync(QueuedMessage::info(source, message));
    }

    pub fn push_batch_sync(&self, messages: Vec<QueuedMessage>) {
        self.queue().push_batch_sync(messages);
    }

    pub fn push_system_reminder_sync(
        &self,
        kind: MessageKind,
        source: MessageSource,
        reminder: TrustedSystemReminder,
    ) {
        self.push_sync(QueuedMessage::system_reminder(kind, source, reminder));
    }

    pub fn push_system_reminder_with_delivery_id_sync(
        &self,
        kind: MessageKind,
        source: MessageSource,
        reminder: TrustedSystemReminder,
        delivery_id: MessageId,
    ) {
        self.push_sync(QueuedMessage::system_reminder_with_delivery_id(
            kind,
            source,
            reminder,
            delivery_id,
        ));
    }
}
