/// 截止后的晚结果不自动复活旧 run；显式 resume 读取同一队列并推理。
#[tokio::test(start_paused = true)]
async fn late_result_after_deadline_requires_explicit_resume() {
    let (child, manager, built, initial_calls, result_calls, ticket) = child_with_nested_task();
    let child_id = child.store().thread_id.clone().unwrap();
    let first = run_sync_subagent(
        &child_id,
        "fixture",
        "/tmp",
        10,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        built,
        child.clone(),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        matches!(first, ChildRunOutcome::HandedOff { .. }),
        "{first:?}"
    );
    settle_nested_task(&child, &manager, &ticket);
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(
        result_calls.load(Ordering::SeqCst),
        0,
        "旧 run 不得自动消费截止后的结果"
    );
    child.queue().push_sync(QueuedMessage::prompt(
        MessageSource::UserInput,
        crate::messages::BaseMessage::human("continue explicitly"),
    ));
    let resumed = nested_context(&child, initial_calls.clone(), result_calls.clone());
    let second = run_sync_subagent(
        &child_id,
        "fixture",
        "/tmp",
        10,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        resumed,
        child.clone(),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(matches!(second, ChildRunOutcome::Settled), "{second:?}");
    assert_eq!(result_calls.load(Ordering::SeqCst), 1);
    assert!(super::super::util::extract_last_ai_text(&child).contains("processed nested result"));
}

/// 无 owner 的嵌入路径在 D 后显式失败，不无限等待。
#[tokio::test(start_paused = true)]
async fn missing_owner_reports_capability_failure() {
    let manager = Arc::new(TaskManager::new());
    let child = make_child(manager.clone(), false);
    TaskManagerPort::register(
        manager.as_ref(),
        BgTaskRegistration {
            task_id: "shell-task".into(),
            kind: BgTaskKind::Shell,
            summary: "permanent".into(),
            pid: None,
            kill: Some(Box::new(|| {})),
        },
    )
    .unwrap();
    child.queue().push_sync(QueuedMessage::prompt(
        MessageSource::UserInput,
        crate::messages::BaseMessage::human("run shell"),
    ));
    let built = build_v2_subagent_context(
        Some(child.clone()),
        Box::new(NestedResultLlm {
            initial_calls: Arc::new(AtomicUsize::new(0)),
            result_calls: Arc::new(AtomicUsize::new(0)),
        }),
        Arc::new(MiddlewareChain::new()),
        Vec::new(),
        Arc::new(|_| true),
        None,
        "/tmp",
        child.config().cancel_token.as_ref().clone(),
        None,
        None,
        None,
        None,
        Some(agent_id_from_child_thread(
            &child.store().thread_id.clone().unwrap(),
        )),
    );
    let outcome = tokio::time::timeout(
        Duration::from_secs(125),
        run_child_until_terminal(built.context, 10, &child),
    )
    .await
    .expect("missing owner must not wait beyond D");
    let ChildRunOutcome::HandoffFailed { failure, .. } = outcome else {
        panic!("expected capability failure: {outcome:?}");
    };
    assert!(failure
        .error
        .to_string()
        .contains("handoff_capability_unavailable"));
}

/// 交接 canonical 写入失败：结构化 HandoffFailed，不返回 HandedOff 或成功。
#[tokio::test(start_paused = true)]
async fn handoff_persistence_failure_is_not_reported_as_handed_off() {
    let manager = Arc::new(TaskManager::new());
    let child_id = uuid::Uuid::now_v7().to_string();
    let child = Session::new(
        Arc::from("/tmp"),
        FrozenContext::builder().build(),
        Some(child_id.clone()),
    );
    let (owner, store) = bind_owner_store(&child, &manager);
    child.set_subagent_host(SubagentHost {
        task_manager: Some(manager.clone()),
        child_owner: Some(owner.clone()),
        ..Default::default()
    });
    store.restrict_to_history_read_only();
    TaskManagerPort::register(
        manager.as_ref(),
        BgTaskRegistration {
            task_id: "shell-task".into(),
            kind: BgTaskKind::Shell,
            summary: "permanent".into(),
            pid: None,
            kill: Some(Box::new(|| {})),
        },
    )
    .unwrap();
    child.queue().push_sync(QueuedMessage::prompt(
        MessageSource::UserInput,
        crate::messages::BaseMessage::human("run shell"),
    ));
    let built = build_v2_subagent_context(
        Some(child.clone()),
        Box::new(NestedResultLlm {
            initial_calls: Arc::new(AtomicUsize::new(0)),
            result_calls: Arc::new(AtomicUsize::new(0)),
        }),
        Arc::new(MiddlewareChain::new()),
        Vec::new(),
        Arc::new(|_| true),
        None,
        "/tmp",
        child.config().cancel_token.as_ref().clone(),
        None,
        None,
        None,
        None,
        Some(agent_id_from_child_thread(&child_id)),
    );
    let permit = permit_for(&child, &built.context);
    let outcome = tokio::time::timeout(
        Duration::from_secs(125),
        run_child_until_terminal(built.context, 10, &child),
    )
    .await
    .expect("persistence failure must not wait beyond D");
    if let Some(permit) = permit {
        permit.finish(Err("handoff persistence failed".into()));
    }
    let ChildRunOutcome::HandoffFailed { failure, .. } = outcome else {
        panic!("持久化失败不得伪装成交接: {outcome:?}");
    };
    assert!(
        failure.error.to_string().contains("persistence")
            || failure
                .error
                .to_string()
                .contains("handoff_capability_unavailable")
    );
    assert!(
        owner.receipt().is_none(),
        "未提交的交接不得留 committed receipt"
    );
    assert!(owner.pending_receipt().is_some(), "冻结身份必须保留供重试");
    assert_eq!(manager.active_count(), 1);
}

struct FixedLlm(&'static str);

#[async_trait::async_trait]
impl ReactLLM for FixedLlm {
    async fn generate_reasoning(
        &self,
        _messages: &[crate::messages::BaseMessage],
        _tools: &[&dyn BaseTool],
        _streaming: Option<StreamingContext>,
    ) -> crate::error::AgentResult<Reasoning> {
        Ok(Reasoning::with_answer("", self.0))
    }
}

/// 旧执行的 ContinueCurrentRun 控制消息不得驱动新 run。
fn fixed_context(session: &Arc<Session>, answer: &'static str) -> V2SubagentContext {
    let id = session.store().thread_id.clone().unwrap();
    build_v2_subagent_context(
        Some(session.clone()),
        Box::new(FixedLlm(answer)),
        Arc::new(MiddlewareChain::new()),
        Vec::new(),
        Arc::new(|_| true),
        None,
        "/tmp",
        session.config().cancel_token.as_ref().clone(),
        None,
        None,
        None,
        None,
        Some(agent_id_from_child_thread(&id)),
    )
}

fn transcript_contains(session: &Session, needle: &str) -> bool {
    session
        .transcript()
        .read()
        .entries()
        .iter()
        .any(|entry| match entry {
            crate::session::TranscriptEntry::Message(message) => message.content().contains(needle),
            crate::session::TranscriptEntry::Reminder { reminder, .. } => {
                reminder.as_reminder().body.contains(needle)
                    || reminder.as_reminder().metadata.to_string().contains(needle)
            }
        })
}

/// 子 Agent 结束时不走 ACP `finish_prompt_turn`，占用快照必须由 child runner
/// 在终态结算前写入 `threads.context_usage_tokens`。
struct UsageSnapshotLlm;

#[async_trait::async_trait]
impl ReactLLM for UsageSnapshotLlm {
    async fn generate_reasoning(
        &self,
        _messages: &[crate::messages::BaseMessage],
        _tools: &[&dyn BaseTool],
        _streaming: Option<StreamingContext>,
    ) -> crate::error::AgentResult<Reasoning> {
        let mut reasoning = Reasoning::with_answer("", "child answer");
        reasoning.usage = Some(peri_model::TokenUsage::new(2048, 16));
        Ok(reasoning)
    }
}

#[tokio::test]
async fn child_terminal_persists_context_usage_snapshot() {
    let manager = Arc::new(TaskManager::new());
    let child_id = uuid::Uuid::now_v7().to_string();
    let child = Session::new(
        Arc::from("/tmp"),
        FrozenContext::builder().build(),
        Some(child_id.clone()),
    );
    let store = crate::session::test_resources::mock::MockSessionResources::new();
    child.set_subagent_host(SubagentHost {
        task_manager: Some(manager),
        session_resources: Some(store.clone()),
        ..Default::default()
    });
    child.queue().push_sync(QueuedMessage::prompt(
        MessageSource::UserInput,
        crate::messages::BaseMessage::human("child work"),
    ));
    let built = build_v2_subagent_context(
        Some(child.clone()),
        Box::new(UsageSnapshotLlm),
        Arc::new(MiddlewareChain::new()),
        Vec::new(),
        Arc::new(|_| true),
        None,
        "/tmp",
        child.config().cancel_token.as_ref().clone(),
        None,
        None,
        None,
        None,
        Some(agent_id_from_child_thread(&child_id)),
    );

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        run_child_until_terminal(built.context, 10, &child),
    )
    .await
    .expect("child run must terminate");
    assert!(matches!(outcome, ChildRunOutcome::Settled), "{outcome:?}");

    let meta = store
        .load_session_meta(&ThreadId::from(child_id))
        .await
        .expect("child meta must be readable after the snapshot write");
    assert_eq!(
        meta.context_usage_tokens,
        Some(2048),
        "child 完成后的真实占用必须已持久化"
    );
}

#[path = "child_runner_background_test.rs"]
mod background;
#[path = "child_runner_deadline_test.rs"]
mod deadline;
#[path = "child_runner_nested_test.rs"]
mod nested;
#[cfg(unix)]
#[path = "child_runner_process_test.rs"]
mod process;
