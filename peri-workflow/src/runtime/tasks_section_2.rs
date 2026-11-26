impl WorkflowTaskTable {
    #[must_use]
    pub fn new(updates: broadcast::Sender<WorkflowResourceUpdate>, gate: Arc<CloseGate>) -> Self {
        Self {
            inner: Mutex::new(TableInner {
                tasks: HashMap::new(),
                order: Vec::new(),
                run_kills: HashMap::new(),
                agent_kills: HashMap::new(),
                unknown_side_effects: std::collections::HashSet::new(),
            }),
            events: Arc::new(BoundedEventLog::new(Self::EVENT_LOG_CAPACITY)),
            updates,
            gate,
            terminal_sink: parking_lot::RwLock::new(None),
        }
    }

    /// 绑定终态通知（`WorkflowRuntime` 构造期一次绑定；见 [`TerminalSink`] 契约）。
    pub fn set_terminal_sink(&self, sink: TerminalSink) {
        self.set_terminal_handler(Arc::new(move |record, _| sink(record)));
    }

    pub(super) fn set_terminal_handler(&self, handler: TerminalHandler) {
        *self.terminal_sink.write() = Some(handler);
    }

    /// 通知终态（幂等路径也调用：重复回填是「可重试投递」的入口，Node 侧按 taskId 去重）。
    fn notify_terminal(&self, record: &WorkflowTaskRecord, first_commit: bool) {
        if let Some(sink) = self.terminal_sink.read().as_ref() {
            sink(record, first_commit);
        }
    }

    /// 任务事件日志容量（保留边界随淘汰前进）。
    const EVENT_LOG_CAPACITY: usize = 512;

    /// 任务事件日志（`workflow://tasks` 的事实源）。
    #[must_use]
    pub fn events(&self) -> &Arc<BoundedEventLog<WorkflowTaskEvent>> {
        &self.events
    }

    /// 发布任务：首个发布提交并追加 `task.published` 事件；重复发布幂等。
    ///
    /// run / agent kill 栅栏已置位时（kill 与发布并发），记录直接落 `cancelled`
    /// 终态：Node 侧的迟到发布不会逃逸出本轮 kill。状态变更与事件追加在同一临界区
    /// 提交（事件顺序 == 状态提交顺序）。
    ///
    /// 关闭门票据覆盖「关闭检查 + 状态提交」整个临界区：关闭返回后到达的发布被拒绝
    /// （不登记、不产生事件），在途发布要么整体在关闭前提交、要么被拒绝
    /// （对抗审查 F6）。
    #[must_use]
    pub fn publish(&self, task_id: String, params: AgentRunParams) -> PublishOutcome {
        let run_id = params.run_id.clone();
        let agent_id = params.agent_id;
        self.publish_inner(
            task_id,
            run_id,
            Some(agent_id),
            WorkflowTaskPayload::Agent { params },
        )
    }

    /// 发布脚本工具调用任务（`tool/task`）。
    ///
    /// 与 agent 任务共用同一张表、kill 栅栏与事件流；不参与 `maxAgents` 准入
    /// （`tool()` 不注册为 agent 任务），也不占用 `maxConcurrency` 槽位。
    #[must_use]
    pub fn publish_tool(
        &self,
        task_id: String,
        run_id: &str,
        tool_name: String,
        arguments: serde_json::Map<String, serde_json::Value>,
        tool_seq: u64,
        agent_seq: u64,
    ) -> PublishOutcome {
        self.publish_inner(
            task_id,
            run_id.to_string(),
            None,
            WorkflowTaskPayload::Tool {
                tool_name,
                arguments,
                tool_seq,
                agent_seq,
            },
        )
    }

    fn publish_inner(
        &self,
        task_id: String,
        run_id: String,
        agent_id: Option<u64>,
        params: WorkflowTaskPayload,
    ) -> PublishOutcome {
        let Some(_ticket) = self.gate.admit() else {
            return PublishOutcome::Closed;
        };
        let mut inner = self.inner.lock();
        Self::trim_locked(&mut inner, now_ms());
        let key = (run_id.clone(), task_id.clone());
        if let Some(existing) = inner.tasks.get(&key) {
            return PublishOutcome::AlreadyKnown(existing.record.clone());
        }
        let now = now_ms();
        // run 级栅栏对两种任务都生效；agent 级栅栏只命中 agent 任务。
        let kill_reason = inner.run_kills.get(&run_id).cloned().or_else(|| {
            agent_id
                .and_then(|agent_id| inner.agent_kills.get(&(run_id.clone(), agent_id)).cloned())
        });
        let record = WorkflowTaskRecord {
            task_id: task_id.clone(),
            run_id: run_id.clone(),
            agent_id,
            state: WorkflowTaskState::Published,
            params,
            published_at_ms: now,
            result: None,
            tool_outcome: None,
            reason: None,
        };
        inner.tasks.insert(
            key.clone(),
            WorkflowTaskEntry {
                record: record.clone(),
                cancel: None,
                claim_id: None,
                terminal_at_ms: None,
            },
        );
        inner.order.push(key.clone());
        self.commit_locked(&mut inner, WorkflowTaskEventKind::Published, &record);
        if let Some(reason) = kill_reason {
            // kill 栅栏命中：同一次临界区内补一条终态事件（published → cancelled）。
            let terminal = self.write_terminal_locked(
                &mut inner,
                &key,
                WorkflowTaskState::Cancelled,
                TerminalPayload::Clear,
                Some(reason.unwrap_or_else(|| "workflow run killed".to_string())),
                now,
            );
            if let Some(record) = terminal {
                return PublishOutcome::Published(record);
            }
        }
        PublishOutcome::Published(record)
    }

    /// 登记执行：`published → executing`，登记取消句柄（唯一 claim）。
    ///
    /// 只有返回 [`ClaimOutcome::Claimed`] 的调用方允许启动执行；已被他人 claim、
    /// 已终态或不存在时调用方不得执行（重试路径按返回值收敛）。
    pub fn mark_executing(
        &self,
        run_id: &str,
        task_id: &str,
        cancel: CancellationToken,
    ) -> ClaimOutcome {
        let mut inner = self.inner.lock();
        let key = (run_id.to_string(), task_id.to_string());
        let Some(entry) = inner.tasks.get_mut(&key) else {
            return ClaimOutcome::NotFound;
        };
        match entry.record.state {
            WorkflowTaskState::Published => {
                entry.record.state = WorkflowTaskState::Executing;
                entry.cancel = Some(cancel);
                let record = entry.record.clone();
                self.commit_locked(&mut inner, WorkflowTaskEventKind::Executing, &record);
                ClaimOutcome::Claimed(record)
            }
            WorkflowTaskState::Executing => ClaimOutcome::AlreadyExecuting(entry.record.clone()),
            _ => ClaimOutcome::AlreadyTerminal(entry.record.clone()),
        }
    }

    /// Atomic, retry-safe MCP claim. Capacity and state are decided by the owner.
    pub fn claim_for_consumer(
        &self,
        run_id: &str,
        task_id: &str,
        claim_id: &str,
        capacity: Option<u32>,
    ) -> Option<WorkflowTaskRecord> {
        let mut inner = self.inner.lock();
        let key = (run_id.to_owned(), task_id.to_owned());
        let entry = inner.tasks.get(&key)?;
        if entry.record.state == WorkflowTaskState::Executing
            && entry.claim_id.as_deref() == Some(claim_id)
        {
            return Some(entry.record.clone());
        }
        if entry.record.state != WorkflowTaskState::Published {
            return None;
        }
        // `maxConcurrency` 只约束引擎 agent 槽位：tool 任务不占槽位，也不参与容量门限。
        if capacity.is_some_and(|limit| {
            entry.record.kind() == WorkflowTaskKind::Agent
                && inner
                    .tasks
                    .values()
                    .filter(|entry| {
                        entry.record.run_id == run_id
                            && entry.record.state == WorkflowTaskState::Executing
                            && entry.record.kind() == WorkflowTaskKind::Agent
                    })
                    .count()
                    >= limit as usize
        }) {
            return None;
        }
        let entry = inner.tasks.get_mut(&key)?;
        entry.record.state = WorkflowTaskState::Executing;
        entry.claim_id = Some(claim_id.to_owned());
        entry.cancel = Some(CancellationToken::new());
        let record = entry.record.clone();
        self.commit_locked(&mut inner, WorkflowTaskEventKind::Executing, &record);
        Some(record)
    }

    /// 单次终态提交（CAS）：只允许一次进入终态，重复提交返回既有终态。
    ///
    /// 非终态 `state` 直接拒绝（[`SettleOutcome::InvalidState`]），不修改记录——
    /// 不依赖 `debug_assert!` 维持运行时不变量。准入条件由 `admission` 显式声明；
    /// 重复回填携带不同结果时保留首次结果并记录告警（design §5.1）。
    #[must_use]
    pub fn settle(
        &self,
        run_id: &str,
        task_id: &str,
        admission: SettleAdmission,
        state: WorkflowTaskState,
        result: Option<AgentRunResult>,
        reason: Option<String>,
    ) -> SettleOutcome {
        self.settle_inner(
            run_id,
            task_id,
            admission,
            state,
            TerminalPayload::Agent(result),
            reason,
        )
    }

    /// tool 任务的单次终态提交（与 agent 任务共用同一 CAS 路径）。
    #[must_use]
    pub fn settle_tool(
        &self,
        run_id: &str,
        task_id: &str,
        admission: SettleAdmission,
        state: WorkflowTaskState,
        outcome: Option<WorkflowToolOutcome>,
        reason: Option<String>,
    ) -> SettleOutcome {
        self.settle_inner(
            run_id,
            task_id,
            admission,
            state,
            TerminalPayload::Tool(outcome),
            reason,
        )
    }

    fn settle_inner(
        &self,
        run_id: &str,
        task_id: &str,
        admission: SettleAdmission,
        state: WorkflowTaskState,
        payload: TerminalPayload,
        reason: Option<String>,
    ) -> SettleOutcome {
        let mut inner = self.inner.lock();
        let key = (run_id.to_string(), task_id.to_string());
        let Some(entry) = inner.tasks.get_mut(&key) else {
            return SettleOutcome::NotFound;
        };
        if !state.is_terminal() {
            return SettleOutcome::InvalidState(entry.record.clone());
        }
        if entry.record.state.is_terminal() {
            if payload.conflicts_with(&entry.record) {
                warn!(
                    target: "workflow",
                    run_id,
                    task_id,
                    first_state = ?entry.record.state,
                    incoming_state = ?state,
                    first_result = payload.existing_summary(&entry.record),
                    incoming_result = payload.incoming_summary(),
                    "workflow task terminal conflict: keeping the first result (idempotent retry or conflicting late delivery)"
                );
            }
            let record = entry.record.clone();
            // 重复回填是「可重试投递」的入口（首次投递可能未达 Node）：再投递既有终态，
            // Node 侧按 taskId 去重（design §5.1）。
            self.notify_terminal(&record, false);
            return SettleOutcome::AlreadyTerminal(record);
        }
        if admission == SettleAdmission::ExecutingOnly
            && entry.record.state != WorkflowTaskState::Executing
        {
            return SettleOutcome::NotAdmitted(entry.record.clone());
        }
        // Fence before the terminal sink can deliver the result to the engine: a
        // generic dead result may trigger an immediate retry. Only an admitted,
        // first terminal result can install the fence; rejected reports cannot.
        let scope_failure = payload.agent_scope_failure();
        if let Some(detail) = &scope_failure {
            inner
                .run_kills
                .insert(run_id.to_string(), Some(detail.clone()));
        }
        let record = self.write_terminal_locked(&mut inner, &key, state, payload, reason, now_ms());
        if let Some(detail) = scope_failure {
            self.cancel_matching_locked(&mut inner, |record| record.run_id == run_id, Some(detail));
        }
        match record {
            Some(record) => SettleOutcome::Settled(record),
            None => SettleOutcome::NotFound,
        }
    }

    /// 取消单个任务：同一临界区内完成「非终态判断 + 写 cancelled + 取取消句柄」，
    /// 只有本次提交成功的调用方才触发句柄。
    ///
    /// 另一路已先进入终态时返回 `AlreadyTerminal` 且**不**触发取消（句柄仍归
    /// 首次结算方所有）：不存在「token 已取出但从未触发」的窗口。
    pub fn cancel(&self, run_id: &str, task_id: &str, reason: Option<String>) -> SettleOutcome {
        let mut inner = self.inner.lock();
        let key = (run_id.to_string(), task_id.to_string());
        let Some(entry) = inner.tasks.get_mut(&key) else {
            return SettleOutcome::NotFound;
        };
        if entry.record.state.is_terminal() {
            return SettleOutcome::AlreadyTerminal(entry.record.clone());
        }
        let marks_unknown = entry.record.state == WorkflowTaskState::Executing
            && entry.record.kind() == WorkflowTaskKind::Tool;
        let token = entry.cancel.take();
        let record = self.write_terminal_locked(
            &mut inner,
            &key,
            WorkflowTaskState::Cancelled,
            TerminalPayload::Clear,
            reason,
            now_ms(),
        );
        if marks_unknown {
            // 执行中的 tool 被取消：调用可能已 dispatch，run 终态标记未知副作用。
            inner.unknown_side_effects.insert(run_id.to_string());
        }
        drop(inner);
        if let Some(token) = token {
            token.cancel();
        }
        match record {
            Some(record) => SettleOutcome::Settled(record),
            None => SettleOutcome::NotFound,
        }
    }

    /// 取消某 run 下全部未终态任务（run kill / 会话关闭路径），返回受影响 taskId。
    ///
    /// 先原子置 run 级 kill 栅栏，再取未终态任务：kill 期间（Node 仍可能）发布的新
    /// 任务命中栅栏，直接落 `cancelled`，不逃逸出本轮 kill。
    pub fn cancel_run(&self, run_id: &str, reason: Option<String>) -> Vec<String> {
        let mut inner = self.inner.lock();
        inner.run_kills.insert(run_id.to_string(), reason.clone());
        self.cancel_matching_locked(&mut inner, |record| record.run_id == run_id, reason)
    }

    /// 取消某 run 下指定 agent 的全部未终态任务（单 agent kill 路径）。
    ///
    /// 与 [`Self::cancel_run`] 同构：先置 agent 级 kill 栅栏，再取消已登记任务。
    pub fn cancel_agent(&self, run_id: &str, agent_id: u64, reason: Option<String>) -> Vec<String> {
        let mut inner = self.inner.lock();
        inner
            .agent_kills
            .insert((run_id.to_string(), agent_id), reason.clone());
        self.cancel_matching_locked(
            &mut inner,
            |record| record.run_id == run_id && record.agent_id == Some(agent_id),
            reason,
        )
    }

    /// 记录该 run 存在不可确认的脚本工具副作用（幂等；不改变任务状态）。
    pub fn mark_unknown_tool_side_effect(&self, run_id: &str) {
        self.inner
            .lock()
            .unknown_side_effects
            .insert(run_id.to_string());
    }

    /// 该 run 是否已记录不可确认的脚本工具副作用。
    #[must_use]
    pub fn run_unknown_side_effects(&self, run_id: &str) -> bool {
        self.inner.lock().unknown_side_effects.contains(run_id)
    }

    /// 回收某 run 的 run / agent 级取消栅栏。
    ///
    /// 仅在 run 退役且生产者结束后调用：此后不会再有迟到发布，栅栏不再承担取消
    /// 约束（#468）。终态任务记录仍按自身保留窗口淘汰，聚合视图不提前丢失摘要。
    pub fn forget_run_fences(&self, run_id: &str) {
        let mut inner = self.inner.lock();
        inner.run_kills.remove(run_id);
        inner.unknown_side_effects.remove(run_id);
        inner
            .agent_kills
            .retain(|(run_id_key, _), _| run_id_key != run_id);
    }

    /// 单次临界区内取消全部命中任务：CAS → 追加事件 → 收集取消句柄（锁外触发）。
    fn cancel_matching_locked(
        &self,
        inner: &mut TableInner,
        predicate: impl Fn(&WorkflowTaskRecord) -> bool,
        reason: Option<String>,
    ) -> Vec<String> {
        let now = now_ms();
        let keys: Vec<(String, String)> = inner
            .order
            .iter()
            .filter(|key| {
                inner.tasks.get(*key).is_some_and(|entry| {
                    !entry.record.state.is_terminal() && predicate(&entry.record)
                })
            })
            .cloned()
            .collect();
        let mut tokens = Vec::new();
        let mut cancelled = Vec::new();
        for key in keys {
            // 取消一个**执行中**的 tool 任务 = MCP 调用可能已 dispatch 且不强行中断：
            // run 终态必须带「未知副作用」（kill 路径的置位来源）。
            let mut marks_unknown = false;
            if let Some(entry) = inner.tasks.get_mut(&key) {
                marks_unknown = entry.record.state == WorkflowTaskState::Executing
                    && entry.record.kind() == WorkflowTaskKind::Tool;
                if let Some(token) = entry.cancel.take() {
                    tokens.push(token);
                }
            }
            if marks_unknown {
                inner.unknown_side_effects.insert(key.0.clone());
            }
            let Some(record) = self.write_terminal_locked(
                inner,
                &key,
                WorkflowTaskState::Cancelled,
                TerminalPayload::Clear,
                reason.clone(),
                now,
            ) else {
                continue;
            };
            cancelled.push(record.task_id);
        }
        for token in tokens {
            token.cancel();
        }
        cancelled
    }

    /// 持锁内的终态写入：更新记录 → 追加事件 → 发订阅唤醒。
    ///
    /// 返回写入后的记录；任务不存在返回 `None`。调用方必须已持有 `inner`。
    fn write_terminal_locked(
        &self,
        inner: &mut TableInner,
        key: &(String, String),
        state: WorkflowTaskState,
        payload: TerminalPayload,
        reason: Option<String>,
        now: i64,
    ) -> Option<WorkflowTaskRecord> {
        let entry = inner.tasks.get_mut(key)?;
        entry.record.state = state;
        match payload {
            TerminalPayload::Agent(result) => {
                entry.record.result = result;
                entry.record.tool_outcome = None;
            }
            TerminalPayload::Tool(outcome) => {
                entry.record.result = None;
                entry.record.tool_outcome = outcome;
            }
            TerminalPayload::Clear => {
                entry.record.result = None;
                entry.record.tool_outcome = None;
            }
        }
        entry.record.reason = reason;
        entry.cancel = None;
        entry.terminal_at_ms = Some(now);
        let record = entry.record.clone();
        self.commit_locked(inner, state.into(), &record);
        Some(record)
    }

    pub fn get(&self, run_id: &str, task_id: &str) -> Option<WorkflowTaskRecord> {
        self.inner
            .lock()
            .tasks
            .get(&(run_id.to_string(), task_id.to_string()))
            .map(|entry| entry.record.clone())
    }

    /// 某 run 下的全部任务（发布顺序）。
    #[must_use]
    pub fn for_run(&self, run_id: &str) -> Vec<WorkflowTaskRecord> {
        let inner = self.inner.lock();
        inner
            .order
            .iter()
            .filter(|(run, _)| run == run_id)
            .filter_map(|key| inner.tasks.get(key).map(|entry| entry.record.clone()))
            .collect()
    }

    /// 某 run 下未终态的 **agent** 任务数（`maxAgents` 门限用；只计数，不 clone
    /// 记录）。tool 任务不是 agent 槽位，不计入。
    ///
    /// 只返回计数：准入判定不需要任务正文。计数与 [`Self::publish`] 分属不同临界区，
    /// 结果仅作为发布前的准入提示；幂等与 kill 栅栏仍由发布路径在自身临界区裁决。
    #[must_use]
    pub fn active_count_for_run(&self, run_id: &str) -> usize {
        let inner = self.inner.lock();
        Self::count_states_locked(&inner, run_id, |record| {
            record.kind() == WorkflowTaskKind::Agent && !record.state.is_terminal()
        })
    }

    /// 某 run 下执行中（`executing`）的 **agent** 任务数（`maxConcurrency` 容量用；
    /// 只计数，不 clone 记录）。
    ///
    /// `maxConcurrency` 只约束引擎 agent 槽位：脚本工具调用不受它约束（issue #407）。
    /// 与 [`Self::mark_executing`] 分属不同临界区：返回值用于容量门限，唯一 claim
    /// 仍由 `mark_executing` 的临界区裁决（计数不构成准入保证）。
    #[must_use]
    pub fn executing_count_for_run(&self, run_id: &str) -> usize {
        let inner = self.inner.lock();
        Self::count_states_locked(&inner, run_id, |record| {
            record.kind() == WorkflowTaskKind::Agent && record.state == WorkflowTaskState::Executing
        })
    }

    /// 某 run 下执行中的 **tool** 任务数（run 终态「未知副作用」判定用）。
    #[must_use]
    pub fn executing_tool_count_for_run(&self, run_id: &str) -> usize {
        let inner = self.inner.lock();
        Self::count_states_locked(&inner, run_id, |record| {
            record.kind() == WorkflowTaskKind::Tool && record.state == WorkflowTaskState::Executing
        })
    }

    /// 持锁内的按 run 状态计数：遍历 `order` 用 `tasks.get` 借用过滤，不 clone 记录。
    fn count_states_locked(
        inner: &TableInner,
        run_id: &str,
        matches: impl Fn(&WorkflowTaskRecord) -> bool,
    ) -> usize {
        inner
            .order
            .iter()
            .filter(|(run, _)| run == run_id)
            .filter(|key| {
                inner
                    .tasks
                    .get(*key)
                    .is_some_and(|entry| matches(&entry.record))
            })
            .count()
    }

    /// 某 run 下指定 agent 的任务（单 agent kill 路径）。
    #[must_use]
    pub fn for_agent(&self, run_id: &str, agent_id: u64) -> Vec<WorkflowTaskRecord> {
        self.for_run(run_id)
            .into_iter()
            .filter(|record| record.agent_id == Some(agent_id))
            .collect()
    }

    /// session 级快照（发布顺序；读取前先按保留窗口淘汰过期终态任务）。
    #[must_use]
    pub fn snapshot(&self) -> Vec<WorkflowTaskRecord> {
        let mut inner = self.inner.lock();
        Self::trim_locked(&mut inner, now_ms());
        inner
            .order
            .iter()
            .filter_map(|key| inner.tasks.get(key).map(|entry| entry.record.clone()))
            .collect()
    }

    /// 全部未终态任务（消费者基线扫描）。
    #[must_use]
    pub fn active(&self) -> Vec<WorkflowTaskRecord> {
        self.baseline(None).active_tasks
    }

    /// 任务基线：未终态任务集合 + **同一临界区**读取的已提交上界
    /// （design §4.3 补读协议第 1/4 条：订阅确认后以全量扫描未终态任务建立基线；
    /// 游标早于保留边界时重新建立快照）。
    ///
    /// `active_tasks` 与 `head_sequence` 在一次表锁内读取：提交路径恒为
    /// 「持表锁 → 追加事件」，因此 `head_sequence` 水位内的状态变更都已在快照中
    /// 可见。事件日志保留窗口淘汰发布事件后，基线仍能完整找回活跃任务——
    /// 只读事件页会把「长期 published、发布事件已被淘汰」的任务永久漏掉
    /// （对抗审查 F3）。`run_id` 为 `None` 时是 session 级全量扫描。
    #[must_use]
    pub fn baseline(&self, run_id: Option<&str>) -> TaskBaseline {
        let mut inner = self.inner.lock();
        Self::trim_locked(&mut inner, now_ms());
        let active_tasks = inner
            .order
            .iter()
            .filter_map(|key| inner.tasks.get(key))
            .filter(|entry| !entry.record.state.is_terminal())
            .filter(|entry| run_id.is_none_or(|id| entry.record.run_id == id))
            .map(|entry| entry.record.clone())
            .collect();
        TaskBaseline {
            active_tasks,
            head_sequence: self.events.head_sequence(),
            truncated_before: self.events.truncated_before(),
        }
    }

    /// 保留窗口淘汰 + 硬上界淘汰（有界内存；只淘汰终态任务，活跃任务永不淘汰）。
    ///
    /// 运行期状态不需要 journal 耐久性：终态任务超过保留窗口即从快照表移除
    /// （事件日志是另一条有界流），与 progress store 的 run 清理窗口同量级。
    fn trim_locked(inner: &mut TableInner, now: i64) {
        let before = inner.tasks.len();
        if before == 0 {
            return;
        }
        inner.tasks.retain(|_, entry| {
            entry
                .terminal_at_ms
                .map(|at| now.saturating_sub(at) < TASK_RETENTION_MS)
                .unwrap_or(true)
        });
        if inner.tasks.len() > MAX_TASK_RECORDS || inner.tasks.len() != before {
            inner.order.retain(|key| inner.tasks.contains_key(key));
        }
        if inner.tasks.len() > MAX_TASK_RECORDS {
            let excess = inner.tasks.len() - MAX_TASK_RECORDS;
            let victims: Vec<(String, String)> = inner
                .order
                .iter()
                .filter(|key| {
                    inner
                        .tasks
                        .get(*key)
                        .is_some_and(|entry| entry.record.state.is_terminal())
                })
                .take(excess)
                .cloned()
                .collect();
            for key in victims {
                inner.tasks.remove(&key);
            }
            inner.order.retain(|key| inner.tasks.contains_key(key));
        }
    }

    /// 持锁内的事件提交：追加任务事件 → 发订阅唤醒（先可见后通知）。
    ///
    /// 与状态变更在同一临界区完成，保证事件顺序 == 状态提交顺序（无锁外反序窗口）。
    /// 调用方必须已持有 `inner`（参数仅用于声明该前置条件）；锁序恒为
    /// 「任务表 → 事件日志」，无反向路径。
    fn commit_locked(
        &self,
        _inner: &mut TableInner,
        kind: WorkflowTaskEventKind,
        record: &WorkflowTaskRecord,
    ) {
        self.events.append_with(|sequence| WorkflowTaskEvent {
            sequence,
            event_id: format!(
                "{}:{}:task.{}",
                record.run_id,
                record.task_id,
                kind.as_str()
            ),
            run_id: record.run_id.clone(),
            task_id: record.task_id.clone(),
            agent_id: record.agent_id,
            kind,
            at_ms: now_ms(),
            task: record.clone(),
        });
        let _ = self.updates.send(WorkflowResourceUpdate::Tasks);
        let _ = self.updates.send(WorkflowResourceUpdate::RunTasks {
            run_id: record.run_id.clone(),
        });
        // 先可见后通知：终态提交已落事件与订阅唤醒，最后通知投递面（`agent/result`）。
        if record.state.is_terminal() {
            self.notify_terminal(record, true);
        }
    }
}

/// 结果摘要（只用于日志，不承载语义）：`ok` / `skipped` / `dead`。
fn result_summary(result: Option<&AgentRunResult>) -> Option<&'static str> {
    match result? {
        AgentRunResult::Ok { .. } => Some("ok"),
        AgentRunResult::Skipped => Some("skipped"),
        AgentRunResult::Dead { .. } => Some("dead"),
    }
}

/// tool 结果摘要（只用于日志）：`ok` / `error`。
fn tool_outcome_summary(outcome: Option<&WorkflowToolOutcome>) -> Option<&'static str> {
    match outcome? {
        WorkflowToolOutcome::Ok { .. } => Some("ok"),
        WorkflowToolOutcome::Error { .. } => Some("error"),
    }
}

/// 终态载荷：agent 结果 / tool 结果 / 无载荷（取消路径）。
enum TerminalPayload {
    Agent(Option<AgentRunResult>),
    Tool(Option<WorkflowToolOutcome>),
    /// 取消：任务表不写结果事实（不是一次执行产出）。
    Clear,
}

impl TerminalPayload {
    /// 重复回填是否与既有终态冲突（同一 task 的重试应等同；不同则以首次为准）。
    fn conflicts_with(&self, record: &WorkflowTaskRecord) -> bool {
        match self {
            Self::Agent(incoming) => incoming
                .as_ref()
                .is_some_and(|incoming| record.result.as_ref() != Some(incoming)),
            Self::Tool(incoming) => incoming
                .as_ref()
                .is_some_and(|incoming| record.tool_outcome.as_ref() != Some(incoming)),
            Self::Clear => false,
        }
    }

    fn existing_summary(&self, record: &WorkflowTaskRecord) -> Option<&'static str> {
        match self {
            Self::Tool(_) => tool_outcome_summary(record.tool_outcome.as_ref()),
            Self::Agent(_) | Self::Clear => result_summary(record.result.as_ref()),
        }
    }

    fn incoming_summary(&self) -> Option<&'static str> {
        match self {
            Self::Agent(result) => result_summary(result.as_ref()),
            Self::Tool(outcome) => tool_outcome_summary(outcome.as_ref()),
            Self::Clear => None,
        }
    }

    /// 子作用域未结清的 agent 结果必须把 run 置失败（tool 结果没有该事实）。
    fn agent_scope_failure(&self) -> Option<String> {
        match self {
            Self::Agent(result) => result
                .as_ref()
                .and_then(AgentRunResult::unsettled_scope_error)
                .map(str::to_owned),
            Self::Tool(_) | Self::Clear => None,
        }
    }
}

/// wall clock 毫秒（事件时间戳，供展示与排序参考；不作为生命周期判定依据）。
pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
#[path = "tasks_test.rs"]
mod tests;

impl Debug for WorkflowTaskTable {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkflowTaskTable")
            .finish_non_exhaustive()
    }
}
