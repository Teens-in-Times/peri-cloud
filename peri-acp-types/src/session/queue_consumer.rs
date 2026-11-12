impl MessageQueue {
    /// 生成本次 Receive 的选择：非用户输入全收，用户输入只收 `send_selection` 选中的。
    ///
    /// 顺序固定为 `(enqueued_at, id)`；`Suppressed` 策略与空 Prompt 唤醒信号
    /// 标记为只删不写历史。最多 [`RECEIVE_BATCH_LIMIT`] 条，且不消费发送选择。
    async fn select_for_receive(&self, permit: &ReceivePermit) -> Result<Vec<MqReceiveItem>> {
        let entries = self.query(MqFilter::default()).await?;
        let selection = self.inner.send_selection.lock().clone();
        let mut items = Vec::new();
        for entry in entries {
            if !is_eligible_for_receive(&entry, &selection) {
                continue;
            }
            let disposition = match entry.policy.disposition(&permit.execution) {
                MessageDisposition::Suppressed => MqReceiveDisposition::Suppress,
                // 空 Prompt（keepgoing）只是唤醒信号：行必须消费，但不能制造空用户气泡。
                _ if is_wake_only(&entry) => MqReceiveDisposition::Suppress,
                MessageDisposition::Process | MessageDisposition::ProjectOnly => {
                    MqReceiveDisposition::Commit
                }
            };
            items.push(MqReceiveItem { entry, disposition });
            if items.len() >= RECEIVE_BATCH_LIMIT {
                break;
            }
        }
        Ok(items)
    }

    /// 原子移交：成功后结清发送选择中已结算的 id，仍有可处理项时补一次唤醒。
    ///
    /// 失败时选择保持原样（未确认移交前不能假装已发送）。提交成功之后的唤醒检查是
    /// best-effort：它只影响「是否立刻再取一次」，不能让已提交的移交变成调用方眼中的失败。
    async fn receive(
        &self,
        permit: ReceivePermit,
        items: Vec<MqReceiveItem>,
    ) -> Result<MqReceiveOutcome> {
        if items.is_empty() {
            return Ok(MqReceiveOutcome { items: Vec::new() });
        }
        let outcome = self
            .inner
            .store
            .mq_receive(&self.inner.thread_id, permit.into_selection(items))
            .await?;
        #[cfg(feature = "test-support")]
        for item in &outcome.items {
            // Conflict 与 Suppressed 一样是「行已删除、未写历史」的终态处置，
            // 这里保留同一份可观察诊断，供旧断言读取。
            if matches!(
                item.status,
                MqReceiveStatus::Suppressed | MqReceiveStatus::Conflict
            ) {
                if let Some(entry) = &item.entry {
                    self.inner
                        .suppressed
                        .lock()
                        .push(QueuedMessage::from_mq_entry(entry.clone()));
                }
            }
        }
        let settled = outcome
            .items
            .iter()
            .filter(|item| item.status != MqReceiveStatus::Missing)
            .map(|item| item.id)
            .collect::<Vec<_>>();
        if !settled.is_empty() {
            self.inner
                .send_selection
                .lock()
                .retain(|id| !settled.contains(id));
        }
        // 移交已经提交：此后的唤醒只是「还有活」的提示，查询失败不能把成功结果
        // 变成错误——调用方会据此丢弃已提交的移交（历史已写、MQ 已删）。
        match self.has_processable_work().await {
            Ok(true) => self.notify_wake(),
            Ok(false) => {}
            Err(error) => tracing::warn!(
                thread_id = %self.inner.thread_id,
                %error,
                "post-receive wake check failed; committed handoff is kept"
            ),
        }
        Ok(outcome)
    }

    /// 取一次 Receive 许可：仅 Agent Loop 与同 thread 生命周期 owner 调用。
    ///
    /// 许可不可 Clone，`receive` 会消费它，防止重放同一次准入。
    fn begin_receive(
        &self,
        purpose: MqReceivePurpose,
        execution: ExecutionBinding,
    ) -> ReceivePermit {
        ReceivePermit { execution, purpose }
    }

    // ── 策略查询（按 MQ 事实判定） ──
    //
    // 这些判定与 [`select_for_receive`](Self::select_for_receive) 共用
    // [`is_eligible_for_receive`]：未发送的用户输入留在待发送区，不能让循环
    // 认为「有可消费工作」而空转。

    pub async fn has_ensure_processing(&self) -> Result<bool> {
        let headers = self
            .inner
            .store
            .mq_headers(&self.inner.thread_id, MqFilter::default())
            .await?;
        let selection = self.inner.send_selection.lock().clone();
        Ok(headers.iter().any(|header| {
            is_header_eligible(header, &selection) && header.policy.ensures_processing()
        }))
    }

    pub async fn has_required_for_run(&self, execution: &ExecutionBinding) -> Result<bool> {
        let headers = self
            .inner
            .store
            .mq_headers(&self.inner.thread_id, MqFilter::default())
            .await?;
        let selection = self.inner.send_selection.lock().clone();
        Ok(headers.iter().any(|header| {
            is_header_eligible(header, &selection)
                && header.policy.disposition(execution) == MessageDisposition::Process
        }))
    }

    pub async fn has_required(&self) -> Result<bool> {
        Ok(self
            .inner
            .store
            .mq_headers(&self.inner.thread_id, MqFilter::default())
            .await?
            .iter()
            .any(|header| header.policy.requirement == super::MessageRequirement::Required))
    }

    /// 按来源查询 required 消息，仅用于诊断；不授予来源调度权。
    pub async fn has_pending_defer(&self, source: &MessageSource) -> Result<bool> {
        Ok(self
            .inner
            .store
            .mq_headers(
                &self.inner.thread_id,
                MqFilter {
                    sources: vec![source.clone()],
                    ..Default::default()
                },
            )
            .await?
            .iter()
            .any(|header| header.policy.ensures_processing()))
    }

    /// 是否仍需建立执行处理 required MQ 消息。
    pub async fn needs_mq_continuation(&self) -> Result<bool> {
        self.has_ensure_processing().await
    }

    pub async fn has_wake_up(&self) -> Result<bool> {
        self.has_ensure_processing().await
    }

    /// 队列是否为空。
    pub async fn is_empty(&self) -> Result<bool> {
        Ok(!self.inner.store.mq_exists(&self.inner.thread_id).await?)
    }

    /// 队列长度。
    pub async fn len(&self) -> Result<usize> {
        Ok(self.inner.store.mq_count(&self.inner.thread_id).await?)
    }

    /// 当前 MQ 的最大 `(enqueued_at, id)`；空队列返回 `None`。
    pub async fn admission_watermark(&self) -> Result<Option<MqWatermark>> {
        Ok(self.inner.store.mq_watermark(&self.inner.thread_id).await?)
    }

    /// 是否在水位之后有需要模型处理的入队项；`None` 表示检查全部。
    pub async fn has_ensure_processing_after(
        &self,
        watermark: &Option<MqWatermark>,
    ) -> Result<bool> {
        let filter = MqFilter {
            after: watermark.clone(),
            ..Default::default()
        };
        let headers = self
            .inner
            .store
            .mq_headers(&self.inner.thread_id, filter)
            .await?;
        let selection = self.inner.send_selection.lock().clone();
        Ok(headers.iter().any(|header| {
            is_header_eligible(header, &selection) && header.policy.ensures_processing()
        }))
    }

    /// 清空本 thread 的全部 MQ 行：只给 rewind 与 thread 生命周期调用。
    ///
    /// 常规消费必须走 [`QueueOwner::receive`]；本方法会丢弃尚未处理的待发事实。
    async fn clear(&self) -> Result<()> {
        self.inner.store.mq_clear(&self.inner.thread_id).await?;
        self.inner.send_selection.lock().clear();
        Ok(())
    }

    // ── 唤醒 ──

    pub fn subscribe_wake(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.wake_version.subscribe()
    }

    /// 请求激活 listener 重新读取一次 MQ：不改变任何队列事实，只补一次唤醒。
    ///
    /// 查询失败时调用方无法判断「有没有工作」；把重查责任交还 listener，避免一次
    /// 瞬时错误消耗掉唯一一次已消费的唤醒。没有 listener 时是空操作。
    pub fn request_recheck(&self) {
        self.notify_wake();
    }

    /// 非破坏性等待可执行消息：先注册通知再检查 MQ，避免丢唤醒。
    ///
    /// 查询失败时直接返回（不声称有工作），由调用方在重查时暴露错误——既不能把失败
    /// 当成「没有消息」永久挂起，也不能伪造唤醒依据。
    pub async fn await_wake(&self) {
        loop {
            let notified = self.inner.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.has_wake_up().await {
                Ok(true) => return,
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(
                        thread_id = %self.inner.thread_id,
                        %error,
                        "MQ wake check failed; returning for caller re-check"
                    );
                    return;
                }
            }
            notified.await;
        }
    }

    /// 只等待绑定到给定执行的 required 消息。
    pub async fn await_wake_for_run(&self, execution: &ExecutionBinding) {
        loop {
            let notified = self.inner.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.has_required_for_run(execution).await {
                Ok(true) => return,
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(
                        thread_id = %self.inner.thread_id,
                        %error,
                        "MQ run wake check failed; returning for caller re-check"
                    );
                    return;
                }
            }
            notified.await;
        }
    }

    fn notify_wake(&self) {
        self.inner.wake.notify_waiters();
        self.inner
            .wake_version
            .send_modify(|version| *version = version.saturating_add(1));
    }

    /// 移交后是否仍有本轮可继续消费的工作；调用方据此补一次唤醒。
    async fn has_processable_work(&self) -> Result<bool> {
        self.has_ensure_processing().await
    }
}
