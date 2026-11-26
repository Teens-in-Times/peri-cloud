/// [回归测试] 终态快照必须有界，避免长会话无限增长。
#[test]
fn test_mailbox_terminal_snapshot_is_bounded() {
    let (mailbox, inbox) = make_mailbox();
    mailbox
        .attach_external_attempt_sync(CancellationToken::new(), false)
        .expect("the attach external attempt sync fixture result is required for this assertion");
    for index in 0..(TERMINAL_CAPACITY + 2) {
        let input = make_input(&mailbox, &format!("终态输入 {index}"));
        mailbox
            .enqueue_sync(&input)
            .expect("the enqueue sync fixture result is required for this assertion");
        mailbox.enter_idle_sync();
        let messages = inbox.queue_for_test().drain_all_sync();
        assert_eq!(messages.len(), 1);
        let id = messages[0]
            .message()
            .expect("the fixture entry must contain a message payload")
            .id();
        mailbox.mark_claimed_sync(&[id]);
        mailbox.mark_delivered_sync(&[id]);
        // 本测试只验证终态展示有界；模拟事件泵立即确认交付日志。
        mailbox.ack_delivery_notification(&input.input_id);
    }

    let snapshot = mailbox.snapshot_sync();
    assert!(snapshot.items.is_empty());
    assert_eq!(snapshot.terminal_items.len(), TERMINAL_CAPACITY);
    assert!(snapshot
        .terminal_items
        .iter()
        .all(|item| item.state == UserInputState::Delivered));
}

/// [回归] 同一 input_id 的并发 enqueue 都通过前置指纹检查时，MQ 的 Existing
/// 只比较正文/原稿；选择不同必须按身份冲突拒绝，不能返回先到者的成功回执。
#[tokio::test]
async fn test_concurrent_enqueue_with_different_plugin_selections_conflicts() {
    let (mailbox, inbox) = make_mailbox();
    let mut first = make_input(&mailbox, "同一输入");
    first.plugin_selections = vec![PluginSelection::new("alpha")];
    mailbox
        .enqueue_sync(&first)
        .expect("first enqueue must be admitted");
    let entry = inbox
        .queue_for_test()
        .query(peri_acp_types::session::MqFilter::default())
        .await
        .expect("MQ row query")
        .into_iter()
        .find(|entry| entry.id.as_uuid().to_string() == first.input_id)
        .expect("first request row must exist");

    assert_eq!(
        super::commands::existing_selection_match(
            peri_acp_types::session::MqEnqueueDisposition::Existing,
            &entry,
            &[PluginSelection::new("beta")],
        )
        .unwrap_err(),
        UserInputQueueError::IdentityConflict,
        "选择不同时不得把先到者的 MQ 行当作本次请求的成功回执"
    );
    assert!(
        super::commands::existing_selection_match(
            peri_acp_types::session::MqEnqueueDisposition::Existing,
            &entry,
            &[PluginSelection::new("alpha")],
        )
        .is_ok(),
        "相同选择的重试必须保持幂等"
    );
    assert!(
        super::commands::existing_selection_match(
            peri_acp_types::session::MqEnqueueDisposition::Inserted,
            &entry,
            &[PluginSelection::new("beta")],
        )
        .is_ok(),
        "新插入的行由本次请求定义，无需比较"
    );
}

/// [回归] `AlreadyHandedOff` 的 entry 来自当前请求，选择比较必须回读邮箱在
/// MQ await 期间写入的记录/终态身份指纹。
#[test]
fn test_already_handed_off_uses_stored_identity_fingerprint() {
    assert!(
        super::commands::ensure_stored_fingerprint(None, 7).is_ok(),
        "没有权威身份时不能凭空判定冲突"
    );
    assert!(
        super::commands::ensure_stored_fingerprint(Some(7), 7).is_ok(),
        "相同指纹的并发重试保持幂等"
    );
    assert_eq!(
        super::commands::ensure_stored_fingerprint(Some(8), 7).unwrap_err(),
        UserInputQueueError::IdentityConflict,
        "已有输入的选择/正文与请求不同时必须拒绝"
    );
}

/// [回归] 取消提交后、take_back 恢复前若对账剪掉 Queued 记录，撤回回执仍须
/// 从被删除的 MQ 行恢复插件选择，不能静默变成空选择。
#[tokio::test]
async fn test_taken_back_selections_survive_pruned_record() {
    let (mailbox, inbox) = make_mailbox();
    let mut input = make_input(&mailbox, "撤回选择");
    input.plugin_selections = vec![PluginSelection::new("alpha")];
    mailbox
        .enqueue_sync(&input)
        .expect("enqueue must be admitted");
    mailbox.state.lock().records.clear();

    let id = MessageId::from(uuid::Uuid::parse_str(&input.input_id).unwrap());
    let MqCancelOutcome::Cancelled(entry) = inbox
        .queue_for_test()
        .cancel_user_input(&id)
        .await
        .expect("cancelled MQ row")
    else {
        panic!("queued input must be cancelled");
    };
    assert_eq!(
        super::commands::taken_back_selections(&entry, None),
        vec![PluginSelection::new("alpha")],
        "记录被剪除时选择必须来自 cancelled MQ 行"
    );

    // 旧版本行可能没有持久化选择：此时保留运行期记录的回退语义。
    let mut legacy = make_input(&mailbox, "旧行撤回");
    legacy.plugin_selections = Vec::new();
    mailbox
        .enqueue_sync(&legacy)
        .expect("legacy enqueue must be admitted");
    let id = MessageId::from(uuid::Uuid::parse_str(&legacy.input_id).unwrap());
    let MqCancelOutcome::Cancelled(entry) = inbox
        .queue_for_test()
        .cancel_user_input(&id)
        .await
        .expect("cancelled legacy MQ row")
    else {
        panic!("legacy queued input must be cancelled");
    };
    assert_eq!(
        super::commands::taken_back_selections(&entry, Some(vec![PluginSelection::new("beta")]),),
        vec![PluginSelection::new("beta")],
        "MQ 行没有选择时回退运行期记录"
    );
}

/// [回归] 对账查询进行期间入队：旧查询结果不得清理新记录，快照 revision 必须覆盖其内容。
#[tokio::test]
async fn refresh_keeps_records_enqueued_while_its_mq_query_is_in_flight() {
    let (mailbox, inbox) = make_mailbox();
    mailbox
        .attach_external_attempt_sync(CancellationToken::new(), false)
        .expect("the attach external attempt sync fixture result is required for this assertion");
    let (entered, release) = inbox.queue_for_test().gate_next_query();

    let refresh = tokio::spawn({
        let mailbox = mailbox.clone();
        async move { mailbox.refresh_from_store().await }
    });
    entered.await.expect("refresh MQ query must start");
    let input = make_input(&mailbox, "查询期间入队");
    let receipt = mailbox
        .enqueue(&input)
        .await
        .expect("the enqueue fixture result is required for this assertion");
    release.send(()).expect("release refresh query");

    refresh
        .await
        .expect("the refresh fixture result is required for this assertion")
        .expect("refresh must succeed");
    let fresh = mailbox.snapshot().await;
    let item = fresh
        .items
        .iter()
        .find(|item| item.input_id == input.input_id)
        .expect("查询期间入队的记录必须保留在投影里");
    assert_eq!(
        item.content,
        MessageContent::text("查询期间入队"),
        "revision 与内容必须取自同一线性化点"
    );
    assert!(
        fresh.revision >= receipt.snapshot.revision,
        "快照 revision 不得低于其内容已包含的入队变更"
    );
}

/// [回归] `MessageQueue::bind()` 必须把查询门转发到 store：否则 `gate_next_query`
/// 在绑定资源的路径上静默失效，调用方会在 `entered` 上一直等待。
#[tokio::test]
async fn bound_queue_forwards_query_gate_to_the_store() {
    let resources = MockSessionResources::new();
    let queue = MessageQueue::bind(resources, "thread-gate".into());
    let (entered, release) = queue.gate_next_query();
    let query = tokio::spawn({
        let queue = queue.clone();
        async move { queue.snapshot().await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), entered)
        .await
        .expect("query gate must reach the bound store")
        .expect("query gate sender dropped");
    release.send(()).expect("release bound query");
    query.await.expect("the query fixture result is required for this assertion").expect("bound queue forwards query gate to the store: the completed fixture operation must provide its result payload");
}

/// [回归] 历史查询无法回答时，`Unknown` 不是「不存在」：待发记录必须保留。
#[tokio::test]
async fn unknown_send_result_keeps_the_input_dispatching() {
    let resources = MockSessionResources::new();
    let (mailbox, queue) = make_mock_mailbox(Arc::clone(&resources));
    mailbox
        .attach_external_attempt(CancellationToken::new(), false)
        .await
        .expect("the attach external attempt fixture result is required for this assertion");
    let input = make_input(&mailbox, "无法确认是否移交");
    mailbox
        .enqueue(&input)
        .await
        .expect("the enqueue fixture result is required for this assertion");
    let id = MessageId::from(
        uuid::Uuid::parse_str(&input.input_id)
            .expect("the parse str fixture result is required for this assertion"),
    );
    assert!(matches!(
        queue
            .cancel_user_input(&id)
            .await
            .expect("the cancel user input fixture result is required for this assertion"),
        MqCancelOutcome::Cancelled(_)
    ));
    resources.fail_mq_history_lookup();

    let error = mailbox
        .dispatch(&make_dispatch(&mailbox, &[&input]))
        .await
        .expect_err("无法确认移交时必须返回错误");
    assert!(matches!(error, UserInputQueueError::Store(_)));
    let (state, handed_off) = record_state(&mailbox, &input.input_id).expect("记录必须保留");
    assert_eq!(state, UserInputState::Dispatching);
    assert!(!handed_off, "Unknown 不能宣称已移交");
    mailbox.reclaim().await;
    assert_eq!(
        record_state(&mailbox, &input.input_id)
            .expect("the input id fixture result is required for this assertion")
            .0,
        UserInputState::Dispatching,
        "MQ 缺失不证明发送未生效，收尾不能把未知记录重置成 Queued"
    );
}

/// [回归] `Unknown` 的错误响应只能读投影：记录保留，恢复 MQ 行后同一 ID 可再次发送。
#[tokio::test]
async fn unknown_error_response_keeps_record_and_retry_dispatches() {
    let resources = MockSessionResources::new();
    let (mailbox, queue) = make_mock_mailbox(Arc::clone(&resources));
    mailbox
        .attach_external_attempt(CancellationToken::new(), false)
        .await
        .expect("the attach external attempt fixture result is required for this assertion");
    let input = make_input(&mailbox, "错误响应后重试");
    mailbox
        .enqueue(&input)
        .await
        .expect("the enqueue fixture result is required for this assertion");
    let id = MessageId::from(
        uuid::Uuid::parse_str(&input.input_id)
            .expect("the parse str fixture result is required for this assertion"),
    );
    assert!(matches!(
        queue
            .cancel_user_input(&id)
            .await
            .expect("the cancel user input fixture result is required for this assertion"),
        MqCancelOutcome::Cancelled(_)
    ));
    resources.fail_mq_history_lookup();

    mailbox
        .dispatch(&make_dispatch(&mailbox, &[&input]))
        .await
        .expect_err("无法确认移交时必须返回错误");
    let (state, handed_off) = record_state(&mailbox, &input.input_id).expect("记录必须保留");
    assert_eq!(state, UserInputState::Dispatching);
    assert!(!handed_off, "Unknown 不能宣称已移交");

    // ACP 错误响应只附加纯投影；这里模拟它不得清理记录或推进 revision。
    let revision = mailbox.snapshot().await.revision;
    let projected = mailbox.snapshot().await;
    assert_eq!(projected.revision, revision, "错误响应不得推进 revision");
    assert!(
        projected
            .items
            .iter()
            .any(|item| item.input_id == input.input_id),
        "错误响应不得把 Unknown 记录从投影中删除"
    );

    // 恢复 MQ 行后重试同一 input_id：记录仍在，dispatch 必须能真正发出。
    let message = peri_acp_types::session::QueuedMessage::prompt(
        MessageSource::UserInput,
        BaseMessage::Human {
            id,
            content: input.content.clone(),
        },
    )
    .with_original_draft(input.original_draft.clone())
    .with_policy(peri_acp_types::session::MessagePolicy::ensure_processing());
    queue
        .enqueue(message)
        .await
        .expect("the enqueue fixture result is required for this assertion");

    let receipt = mailbox
        .dispatch(&make_dispatch(&mailbox, &[&input]))
        .await
        .expect("恢复待发行后重试必须成功");
    assert_eq!(receipt.results[0].state, UserInputState::Dispatching);
    let (state, handed_off) = record_state(&mailbox, &input.input_id).expect("记录仍然存在");
    assert_eq!(state, UserInputState::Dispatching);
    assert!(handed_off, "确认接纳后必须标记已移交");
}

/// [回归] 对账只在查询成功并确认 MQ 行消失时清理 Queued。
#[tokio::test]
async fn refresh_prunes_only_confirmed_mq_absence() {
    let resources = MockSessionResources::new();
    let (mailbox, queue) = make_mock_mailbox(Arc::clone(&resources));
    mailbox
        .attach_external_attempt(CancellationToken::new(), false)
        .await
        .expect("the attach external attempt fixture result is required for this assertion");
    let kept = make_input(&mailbox, "仍在 MQ");
    let removed = make_input(&mailbox, "已从 MQ 消失");
    mailbox
        .enqueue(&kept)
        .await
        .expect("the enqueue fixture result is required for this assertion");
    mailbox
        .enqueue(&removed)
        .await
        .expect("the enqueue fixture result is required for this assertion");
    let removed_id = MessageId::from(
        uuid::Uuid::parse_str(&removed.input_id)
            .expect("the parse str fixture result is required for this assertion"),
    );
    assert!(matches!(
        queue
            .cancel_user_input(&removed_id)
            .await
            .expect("the cancel user input fixture result is required for this assertion"),
        MqCancelOutcome::Cancelled(_)
    ));

    mailbox
        .refresh_from_store()
        .await
        .expect("成功查询必须完成对账");
    let snapshot = mailbox.snapshot().await;
    assert!(
        snapshot
            .items
            .iter()
            .any(|item| item.input_id == kept.input_id),
        "查询结果中的记录必须保留"
    );
    assert!(
        snapshot
            .items
            .iter()
            .all(|item| item.input_id != removed.input_id),
        "成功查询确认不存在的 Queued 才允许清理"
    );
}

/// [回归] 实例重建后从 MQ 行恢复插件选择，快照标签不丢。
#[tokio::test]
async fn hydrate_restores_plugin_selections_from_store() {
    let resources = MockSessionResources::new();
    let (first, _queue) = make_mock_mailbox(Arc::clone(&resources));
    first
        .attach_external_attempt(CancellationToken::new(), false)
        .await
        .expect("the attach external attempt fixture result is required for this assertion");
    let mut input = make_input(&first, "选择插件");
    input.plugin_selections = vec![
        PluginSelection::new("alpha"),
        PluginSelection::new("beta@market"),
    ];
    first
        .enqueue(&input)
        .await
        .expect("the enqueue fixture result is required for this assertion");

    // 新实例只从 MQ 查询恢复，不依赖上一实例的内存记录。
    let (restarted, _queue) = make_mock_mailbox(resources);
    restarted
        .hydrate()
        .await
        .expect("the hydrate fixture result is required for this assertion");
    let snapshot = restarted.snapshot().await;
    let item = snapshot
        .items
        .iter()
        .find(|item| item.input_id == input.input_id)
        .expect("待发行必须在重建后恢复");
    assert_eq!(
        item.plugin_selections, input.plugin_selections,
        "选择随 input_id 持久化，重启后仍要生成标签与指引"
    );
}

/// [回归] 对账查询失败时返回错误且不修改记录；最后一次成功状态保留。
#[tokio::test]
async fn refresh_query_failure_keeps_state_and_returns_error() {
    let resources = MockSessionResources::new();
    let (mailbox, _queue) = make_mock_mailbox(Arc::clone(&resources));
    mailbox
        .attach_external_attempt(CancellationToken::new(), false)
        .await
        .expect("the attach external attempt fixture result is required for this assertion");
    let input = make_input(&mailbox, "查询失败");
    mailbox
        .enqueue(&input)
        .await
        .expect("the enqueue fixture result is required for this assertion");
    let before = mailbox.snapshot().await;

    resources.fail_next_mq_queries(1);
    let error = mailbox
        .refresh_from_store()
        .await
        .expect_err("查询失败必须返回错误");
    assert!(matches!(error, UserInputQueueError::Store(_)));
    let after = mailbox.snapshot().await;
    assert_eq!(after.revision, before.revision, "失败不得推进 revision");
    assert!(
        after
            .items
            .iter()
            .any(|item| item.input_id == input.input_id),
        "失败不得删除最后已知记录"
    );

    mailbox
        .refresh_from_store()
        .await
        .expect("恢复后的查询必须成功");
    assert!(
        mailbox
            .snapshot()
            .await
            .items
            .iter()
            .any(|item| item.input_id == input.input_id),
        "成功查询仍能看到 MQ 中的记录"
    );
}

/// [回归] `send_ready_ids` 不得把 `Unknown` 记录标成已移交。
#[tokio::test]
async fn send_ready_ids_does_not_claim_handoff_for_unknown() {
    let resources = MockSessionResources::new();
    let (mailbox, queue) = make_mock_mailbox(Arc::clone(&resources));
    mailbox
        .attach_external_attempt(CancellationToken::new(), false)
        .await
        .expect("the attach external attempt fixture result is required for this assertion");
    let input = make_input(&mailbox, "idle 无法确认");
    mailbox
        .enqueue(&input)
        .await
        .expect("the enqueue fixture result is required for this assertion");
    let id = MessageId::from(
        uuid::Uuid::parse_str(&input.input_id)
            .expect("the parse str fixture result is required for this assertion"),
    );
    assert!(matches!(
        queue
            .cancel_user_input(&id)
            .await
            .expect("the cancel user input fixture result is required for this assertion"),
        MqCancelOutcome::Cancelled(_)
    ));
    resources.fail_mq_history_lookup();

    mailbox.enter_idle().await;

    let (state, handed_off) = record_state(&mailbox, &input.input_id).expect("记录必须保留");
    assert_eq!(state, UserInputState::Dispatching);
    assert!(!handed_off, "Unknown 不是移交结论");
    assert!(
        mailbox.state.lock().ready.contains(&input.input_id),
        "Unknown 必须保留重试资格"
    );
}

/// 空闲会话在 dispatch 的有界窗口内完成真实 Receive 时，本次调用应确认移交。
#[tokio::test]
async fn dispatch_confirms_handoff_on_an_idle_session() {
    let queue = Arc::new(MessageQueue::new());
    let inbox = Arc::new(SessionInbox::new(Arc::clone(&queue)));
    let mailbox = UserInputMailbox::new("session".into(), inbox, Arc::new(|_| {}));
    let input = make_input(&mailbox, "空闲确认");
    mailbox
        .attach_external_attempt(CancellationToken::new(), false)
        .await
        .expect("the attach external attempt fixture result is required for this assertion");
    mailbox
        .enqueue(&input)
        .await
        .expect("the enqueue fixture result is required for this assertion");
    mailbox.stop().await;

    // 真实 Receive：选中后停在提交门，由独立任务放行，模拟空闲会话的移交。
    let (entered, release) = queue.gate_next_receive();
    let worker_queue = Arc::clone(&queue);
    let worker = tokio::spawn(async move {
        loop {
            let owner = worker_queue.owner();
            let permit = owner.begin_receive(
                MqReceivePurpose::ModelLoop,
                ExecutionBinding {
                    turn_id: TurnId::new(),
                    attempt_id: AttemptId::new(),
                },
            );
            let items = owner
                .select_for_receive(&permit)
                .await
                .expect("the select for receive fixture result is required for this assertion");
            if !items.is_empty() {
                return owner
                    .receive(permit, items)
                    .await
                    .expect("the receive fixture result is required for this assertion");
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    });
    let releaser = tokio::spawn(async move {
        entered.await.expect("receive must reach the commit gate");
        tokio::time::sleep(Duration::from_millis(5)).await;
        release.send(()).expect("release receive gate");
    });

    let receipt = mailbox
        .dispatch(&make_dispatch(&mailbox, &[&input]))
        .await
        .expect("the dispatch fixture result is required for this assertion");
    assert_eq!(receipt.results[0].state, UserInputState::Dispatching);
    assert!(
        queue
            .snapshot()
            .await
            .expect("the snapshot fixture result is required for this assertion")
            .is_empty(),
        "dispatch 返回时必须已完成移交"
    );
    assert!(
        record_state(&mailbox, &input.input_id)
            .expect("the input id fixture result is required for this assertion")
            .1,
        "确认移交后记录必须标记 handed_off"
    );

    tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .expect("receive worker must finish")
        .expect("receive worker panicked");
    releaser
        .await
        .expect("the releaser fixture result is required for this assertion");
}

/// [steer 阶段门] 移交确认迟到时不能取消已经包含该输入的请求。
///
/// `dispatch` 的有界确认可以在 Receive 摄取输入、下一次 Reason 打开请求窗口
/// 之后才看到 `HandedOff`。此时该输入已进入当次请求上下文，取消只会让同一
/// 请求白跑一遍；阶段门必须以「接纳是否已被摄取」为准，而不是只看窗口。
#[tokio::test]
async fn test_late_handoff_confirmation_does_not_cancel_consuming_request() {
    let queue = Arc::new(MessageQueue::new());
    let inbox = Arc::new(SessionInbox::new(Arc::clone(&queue)));
    let mailbox = UserInputMailbox::new("session".into(), inbox, Arc::new(|_| {}));
    let cancel = CancellationToken::new();
    let ticket = mailbox
        .attach_external_attempt(cancel.clone(), false)
        .await
        .expect("the attach external attempt fixture result is required for this assertion");
    let input = make_input(&mailbox, "批次内输入");
    mailbox
        .enqueue(&input)
        .await
        .expect("the enqueue fixture result is required for this assertion");

    let mut wake = queue.subscribe_wake();
    let (entered, release) = queue.gate_next_receive();
    let dispatch = {
        let mailbox = Arc::clone(&mailbox);
        let request = make_dispatch(&mailbox, &[&input]);
        tokio::spawn(async move { mailbox.dispatch(&request).await })
    };
    tokio::time::timeout(Duration::from_secs(2), wake.changed())
        .await
        .expect("dispatch 必须先提交发送选择")
        .expect("wake channel must stay open");

    // 模拟工具批次结束后的 Receive：选中后停在提交门，由测试放行。
    let worker_queue = Arc::clone(&queue);
    let worker = tokio::spawn(async move {
        let owner = worker_queue.owner();
        let permit = owner.begin_receive(
            MqReceivePurpose::ModelLoop,
            ExecutionBinding {
                turn_id: TurnId::new(),
                attempt_id: AttemptId::new(),
            },
        );
        let items = owner
            .select_for_receive(&permit)
            .await
            .expect("the select for receive fixture result is required for this assertion");
        assert_eq!(items.len(), 1, "只摄取被挑选的发送输入");
        owner
            .receive(permit, items)
            .await
            .expect("the receive fixture result is required for this assertion")
    });
    tokio::time::timeout(Duration::from_secs(2), entered)
        .await
        .expect("Receive 必须进入提交门")
        .expect("receive gate must stay open");
    release.send(()).expect("release receive gate");
    let outcome = tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .expect("receive worker must finish")
        .expect("receive worker panicked");
    assert_eq!(outcome.items.len(), 1, "输入必须已写入历史");

    // 摄取完成后才开始下一次模型请求；迟到的移交确认不得再取消它。
    let input_id = MessageId::from(
        uuid::Uuid::parse_str(&input.input_id)
            .expect("the parse str fixture result is required for this assertion"),
    );
    mailbox.mark_claimed(&[input_id]).await;
    let gate = mailbox.begin_model_request();
    let receipt = dispatch
        .await
        .expect("dispatch task must finish")
        .expect("the dispatch fixture result is required for this assertion");
    assert_eq!(receipt.results[0].state, UserInputState::Claimed);
    assert!(!cancel.is_cancelled(), "已摄取输入不能触发硬取消");
    assert!(
        !mailbox.is_steer_interruption(&ticket),
        "已摄取输入不能记录为 steer 中断"
    );
    drop(gate);
}

/// [steer 阶段门] Stop 复位与并发 dispatch 的接纳/撤回必须有序。
///
/// Stop 收尾复位记录、撤回发送选择时，并发的 dispatch 可能刚好提交接纳。
/// 复位之后的接纳不能被收尾撤回删除，也不能在记录已回到 Queued 后残留
/// `handed_off`；记录必须保持可重新发送、可再次被 Receive 领取。
#[tokio::test]
async fn test_stop_reset_and_concurrent_dispatch_leave_record_reselectable() {
    let queue = Arc::new(MessageQueue::new());
    let inbox = Arc::new(SessionInbox::new(Arc::clone(&queue)));
    let mailbox = UserInputMailbox::new("session".into(), inbox, Arc::new(|_| {}));
    let cancel = CancellationToken::new();
    mailbox
        .attach_external_attempt(cancel.clone(), false)
        .await
        .expect("the attach external attempt fixture result is required for this assertion");
    let input = make_input(&mailbox, "收尾中的重新发送");
    mailbox
        .enqueue(&input)
        .await
        .expect("the enqueue fixture result is required for this assertion");

    let mut wake = queue.subscribe_wake();
    let dispatch = {
        let mailbox = Arc::clone(&mailbox);
        let request = make_dispatch(&mailbox, &[&input]);
        tokio::spawn(async move { mailbox.dispatch(&request).await })
    };
    tokio::time::timeout(Duration::from_secs(2), wake.changed())
        .await
        .expect("dispatch 必须先提交发送选择")
        .expect("wake channel must stay open");

    // 接纳已提交、移交确认尚未完成时执行 Stop：复位与撤回发生在确认之前。
    mailbox.stop().await;
    let receipt = dispatch
        .await
        .expect("dispatch task must finish")
        .expect("the dispatch fixture result is required for this assertion");
    assert_eq!(
        receipt.results[0].state,
        UserInputState::Queued,
        "复位后的接纳必须回到待发"
    );
    assert_eq!(
        record_state(&mailbox, &input.input_id),
        Some((UserInputState::Queued, false)),
        "回到待发的记录不能残留 handed_off"
    );

    // 重新发送必须能再次被挑选并摄取。
    let resent = mailbox
        .dispatch(&make_dispatch(&mailbox, &[&input]))
        .await
        .expect("the dispatch fixture result is required for this assertion");
    assert_eq!(resent.results[0].state, UserInputState::Dispatching);
    let received = queue.drain_all_sync();
    assert_eq!(received.len(), 1, "重新发送的输入必须被 Receive 领取");
    assert_eq!(
        received[0]
            .message()
            .expect("the fixture entry must contain a message payload")
            .id()
            .as_uuid()
            .to_string(),
        input.input_id
    );
    assert!(cancel.is_cancelled(), "Stop 必须先取消旧执行");
}
