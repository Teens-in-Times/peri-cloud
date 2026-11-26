/// [回归] 显式派发失败由认领补偿统一撤回本次 prompt：只撤回一次，且不触碰
/// 预先存在的其他 prompt。
#[tokio::test]
async fn test_resume_dispatch_failure_withdraws_prompt_once_preserving_others() {
    use peri_acp_types::session::MqFilter;
    use peri_acp_types::session_resources::SessionResources as _;

    let store = MockSessionResources::new();
    let thread_id = uuid::Uuid::now_v7().to_string();
    crate::session::subagent::tests::preset_resumable_thread(&store, &thread_id, None).await;
    let queue =
        crate::session::MessageQueue::bind(store.clone(), ThreadId::from(thread_id.as_str()));
    let preexisting = crate::session::QueuedMessage::new(
        crate::session::MessageKind::Prompt,
        MessageSource::UserInput,
        BaseMessage::human("preexisting instruction"),
    );
    let preexisting_id = preexisting.id();
    queue
        .enqueue_sent(preexisting)
        .await
        .expect("the preexisting prompt fixture result is required for this assertion");
    let mut config = crate::session::subagent::tests::resume_config_with(
        store.clone(),
        thread_id.clone(),
        crate::session::test_resources::mock::model::fixture_source(
            Arc::new(EchoLLM),
            "fixture-scripted",
        ),
        None,
        None,
    );
    config.prompt = Some("retry-once-only".into());

    let error = crate::session::subagent::tests::resume_err(None, config).await;
    assert!(
        error.contains("Background tasks not available"),
        "dispatch 失败必须原样交给调用方: {error}"
    );
    let rows = store
        .mq_query(&ThreadId::from(thread_id.as_str()), MqFilter::default())
        .await
        .expect("the MQ query fixture result is required for this assertion");
    assert_eq!(rows.len(), 1, "只应保留预先存在的 prompt: {rows:?}");
    assert_eq!(rows[0].id, preexisting_id);
    let cancelled = store.take_mq_cancel_ids();
    assert_eq!(
        cancelled.len(),
        1,
        "显式 Err 补偿必须恰好撤回本次 prompt 一次: {cancelled:?}"
    );
    assert_ne!(cancelled[0], preexisting_id);
    assert!(rows.iter().all(|row| row.id != cancelled[0]));
}

/// [回归] 调用方在撤回进行中被取消时，owned 补偿仍须完成撤回与回滚，不能留下
/// 「prompt 仍在队列、状态已回滚」的重试重复投递窗口。
#[tokio::test]
async fn test_resume_compensation_survives_caller_cancel_during_withdrawal() {
    use std::sync::atomic::AtomicBool;

    let store = MockSessionResources::new();
    let thread_id = uuid::Uuid::now_v7().to_string();
    crate::session::subagent::tests::preset_resumable_thread(&store, &thread_id, None).await;
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    *store
        .mq_cancel_gate
        .lock()
        .expect("mq cancel gate mutex must remain unpoisoned") = Some(ResumeLoadGate {
        entered: entered_tx,
        release: release_rx,
        dropped: Arc::new(AtomicBool::new(false)),
    });
    let mut config = crate::session::subagent::tests::resume_config_with(
        store.clone(),
        thread_id.clone(),
        crate::session::test_resources::mock::model::fixture_source(
            Arc::new(EchoLLM),
            "fixture-scripted",
        ),
        None,
        None,
    );
    config.prompt = Some("cancel-during-withdrawal".into());

    let mut caller = Box::pin(SessionFactory::resume_subagent(None, config));
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::select! {
            entered = entered_rx => entered.expect("withdrawal must begin"),
            result = &mut caller => panic!(
                "resume ended before the withdrawal gate: {}",
                result.is_ok()
            ),
        }
    })
    .await
    .expect("resume must reach the withdrawal gate");
    // 调用方在撤回 await 中被取消：owned task 必须继续完成撤回与回滚。
    drop(caller);
    release_tx
        .send(())
        .expect("owned compensation must still be waiting");

    wait_resume_compensated(&store, &thread_id).await;
}

/// [回归] 撤回结果未确认时不得回滚认领：首次 cancel 返回提交未知后，认领保持
/// Active；只有重查确认行仍在、重试成功提交后才回滚。
#[tokio::test]
async fn test_resume_compensation_defers_rollback_until_withdrawal_commits() {
    use peri_acp_types::session::MqFilter;
    use peri_acp_types::session_resources::SessionResources as _;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    let store = MockSessionResources::new();
    let thread_id = uuid::Uuid::now_v7().to_string();
    crate::session::subagent::tests::preset_resumable_thread(&store, &thread_id, None).await;
    store.mq_cancel_failures.store(1, Ordering::SeqCst);
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    *store
        .mq_cancel_gate
        .lock()
        .expect("mq cancel gate mutex must remain unpoisoned") = Some(ResumeLoadGate {
        entered: entered_tx,
        release: release_rx,
        dropped: Arc::new(AtomicBool::new(false)),
    });
    let mut config = crate::session::subagent::tests::resume_config_with(
        store.clone(),
        thread_id.clone(),
        crate::session::test_resources::mock::model::fixture_source(
            Arc::new(EchoLLM),
            "fixture-scripted",
        ),
        None,
        None,
    );
    config.prompt = Some("defer-rollback".into());

    let mut caller = Box::pin(SessionFactory::resume_subagent(None, config));
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::select! {
            entered = entered_rx => entered.expect("withdrawal retry must begin"),
            result = &mut caller => panic!(
                "resume ended before the withdrawal retry gate: {}",
                result.is_ok()
            ),
        }
    })
    .await
    .expect("resume must reach the withdrawal retry gate");

    assert_eq!(
        store
            .load_meta(&thread_id)
            .await
            .expect("the load meta fixture result is required for this assertion")
            .agent_status,
        AgentStatus::Active,
        "撤回未提交时不得回滚认领"
    );
    assert_eq!(
        store
            .mq_query(&ThreadId::from(thread_id.as_str()), MqFilter::default())
            .await
            .expect("the MQ query fixture result is required for this assertion")
            .len(),
        1,
        "提交未知的首次撤回不得被当作已删除"
    );

    release_tx
        .send(())
        .expect("compensation retry must be waiting");
    let error = caller.await.expect_err("后台任务不可用时 resume 必须失败");
    assert!(
        error.to_string().contains("Background tasks not available"),
        "必须保留原始 dispatch 错误: {error}"
    );
    wait_resume_compensated(&store, &thread_id).await;
}
