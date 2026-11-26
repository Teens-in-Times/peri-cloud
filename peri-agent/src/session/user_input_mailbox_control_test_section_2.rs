#[test]
fn test_mailbox_stop_then_late_suspend_waits_for_fresh_attempt() {
    let (mailbox, inbox) = crate::session::user_input_mailbox::tests::make_mailbox();
    let current = mailbox
        .attach_external_attempt_sync(CancellationToken::new(), false)
        .expect("the attach external attempt sync fixture result is required for this assertion");
    let waiting = crate::session::user_input_mailbox::tests::make_input(&mailbox, "停止前待办");
    mailbox
        .enqueue_sync(&waiting)
        .expect("the enqueue sync fixture result is required for this assertion");
    mailbox.stop_sync();
    // 旧 loop 可能已越过取消检查，迟到报告挂起。
    mailbox.enter_idle_sync();
    assert!(
        inbox.queue_for_test().is_empty_sync(),
        "迟到 idle 不能绕过 Stop"
    );
    assert_eq!(
        mailbox.snapshot_sync().items[0].state,
        UserInputState::Queued
    );
    let resumed =
        crate::session::user_input_mailbox::tests::make_input(&mailbox, "用户明确重新提交");
    mailbox
        .enqueue_sync(&resumed)
        .expect("the enqueue sync fixture result is required for this assertion");
    assert!(
        inbox.queue_for_test().is_empty_sync(),
        "新输入不能唤醒已被 Stop 的旧执行"
    );

    mailbox.finish_attempt_sync(&current, UserInputAttemptOutcome::Interrupted);
    let next = mailbox.reserve_run().expect("明确恢复必须保留新执行的准入");
    assert!(mailbox.attach_attempt_sync(&next, CancellationToken::new()));
    let delivered = inbox.queue_for_test().drain_all_sync();
    assert_eq!(delivered.len(), 1, "显式恢复只交接第一条原待办");
    assert_eq!(
        delivered[0]
            .message()
            .expect("the fixture entry must contain a message payload")
            .id()
            .as_uuid()
            .to_string(),
        waiting.input_id
    );
    let delivered_id = delivered[0]
        .message()
        .expect("the fixture entry must contain a message payload")
        .id();
    mailbox.mark_claimed_sync(&[delivered_id]);
    mailbox.mark_delivered_sync(&[delivered_id]);
    assert_eq!(
        mailbox.snapshot_sync().items[0].input_id,
        resumed.input_id,
        "新输入保留在原待办之后"
    );
    assert_eq!(
        mailbox.snapshot_sync().items[0].state,
        UserInputState::Queued
    );
}

/// [steer 阶段门] Stop 收尾中的重新发送是明确继续：不再取消（令牌已由 Stop
/// 取消），但必须把 reason 从 Stop 改为 Steer，宿主据此保留续跑。
#[test]
fn test_mailbox_dispatch_after_stop_marks_explicit_continuation() {
    let (mailbox, inbox) = crate::session::user_input_mailbox::tests::make_mailbox();
    let cancel = CancellationToken::new();
    let current = mailbox
        .attach_external_attempt_sync(cancel.clone(), false)
        .expect("the attach external attempt sync fixture result is required for this assertion");
    let stopped = crate::session::user_input_mailbox::tests::make_input(&mailbox, "停止前待办");
    mailbox
        .enqueue_sync(&stopped)
        .expect("the enqueue sync fixture result is required for this assertion");
    mailbox.stop_sync();
    assert!(cancel.is_cancelled(), "Stop 必须先取消旧执行");

    let resent = crate::session::user_input_mailbox::tests::make_input(&mailbox, "停止后重新发送");
    mailbox
        .enqueue_sync(&resent)
        .expect("the enqueue sync fixture result is required for this assertion");
    mailbox
        .dispatch_sync(&crate::session::user_input_mailbox::tests::make_dispatch(
            &mailbox,
            &[&resent],
        ))
        .expect("the dispatch sync fixture result is required for this assertion");
    assert!(
        mailbox.is_steer_interruption(&current),
        "Stop 收尾中的重新发送必须标记为显式继续"
    );

    mailbox.finish_attempt_sync(&current, UserInputAttemptOutcome::Interrupted);
    let continuation = mailbox.reserve_run().expect("明确继续必须保留续跑准入");
    assert!(mailbox.attach_attempt_sync(&continuation, CancellationToken::new()));
    let delivered = inbox.queue_for_test().drain_all_sync();
    let delivered_ids: Vec<String> = delivered
        .iter()
        .map(|entry| {
            entry
                .message()
                .expect("the fixture entry must contain a message payload")
                .id()
                .as_uuid()
                .to_string()
        })
        .collect();
    assert_eq!(
        delivered_ids,
        [stopped.input_id.clone(), resent.input_id.clone()],
        "续跑按队列顺序领取旧待办与 Stop 后重新发送的输入"
    );
}

/// [回归测试] Stop 完成后，新的后台 continuation 仍继承 paused；新输入必须
/// 解除暂停并交接给这个仍有效、未取消的当前执行。
#[test]
fn test_mailbox_new_input_wakes_background_attempt_after_finished_stop() {
    let (mailbox, inbox) = crate::session::user_input_mailbox::tests::make_mailbox();
    let old = mailbox
        .attach_external_attempt_sync(CancellationToken::new(), false)
        .expect("the attach external attempt sync fixture result is required for this assertion");
    mailbox.stop_sync();
    mailbox.finish_attempt_sync(&old, UserInputAttemptOutcome::Interrupted);

    let background = mailbox
        .attach_external_attempt_sync(CancellationToken::new(), false)
        .expect("the attach external attempt sync fixture result is required for this assertion");
    mailbox.enter_idle_sync();

    let input = crate::session::user_input_mailbox::tests::make_input(&mailbox, "停止后重新发送");
    mailbox
        .enqueue_sync(&input)
        .expect("the enqueue sync fixture result is required for this assertion");

    assert_eq!(mailbox.active_run_ticket(), Some(background));
    assert!(
        mailbox.reserve_run().is_none(),
        "已有执行时不能启动第二个执行"
    );
    let received = inbox.queue_for_test().drain_all_sync();
    assert_eq!(received.len(), 1, "应交接给当前未取消的后台执行");
    assert_eq!(
        received[0]
            .message()
            .expect("the fixture entry must contain a message payload")
            .id()
            .as_uuid()
            .to_string(),
        input.input_id
    );
}

/// [回归测试] 输入先于新后台执行的 idle 边界完成 promotion；enter_idle 必须
/// 交接已存在的 ready，而不是要求本次调用重新 promote。
#[test]
fn test_mailbox_ready_input_is_handed_off_when_external_attempt_enters_idle() {
    let (mailbox, inbox) = crate::session::user_input_mailbox::tests::make_mailbox();
    let input = crate::session::user_input_mailbox::tests::make_input(&mailbox, "已经进入 ready");
    mailbox
        .enqueue_sync(&input)
        .expect("the enqueue sync fixture result is required for this assertion");

    let background = mailbox
        .attach_external_attempt_sync(CancellationToken::new(), false)
        .expect("the attach external attempt sync fixture result is required for this assertion");

    assert!(mailbox.reserve_run().is_none());
    mailbox.enter_idle_sync();

    assert_eq!(mailbox.active_run_ticket(), Some(background));
    let received = inbox.queue_for_test().drain_all_sync();
    assert_eq!(received.len(), 1, "已有 ready 不能因未再次 promote 被跳过");
    assert_eq!(
        received[0]
            .message()
            .expect("the fixture entry must contain a message payload")
            .id()
            .as_uuid()
            .to_string(),
        input.input_id
    );
}

/// [回归测试] enqueue 先完成 promotion、随后后台执行才 attach 的竞争：
/// reserve 被拒后，现有执行进入 idle 仍要消费已 ready 的输入。
#[test]
fn test_mailbox_ready_input_survives_attach_before_idle() {
    let (mailbox, inbox) = crate::session::user_input_mailbox::tests::make_mailbox();
    let old = mailbox
        .attach_external_attempt_sync(CancellationToken::new(), false)
        .expect("the attach external attempt sync fixture result is required for this assertion");
    mailbox.stop_sync();
    mailbox.finish_attempt_sync(&old, UserInputAttemptOutcome::Interrupted);

    let input =
        crate::session::user_input_mailbox::tests::make_input(&mailbox, "attach 前已 ready");
    mailbox
        .enqueue_sync(&input)
        .expect("the enqueue sync fixture result is required for this assertion");
    let background = mailbox
        .attach_external_attempt_sync(CancellationToken::new(), false)
        .expect("the attach external attempt sync fixture result is required for this assertion");

    assert!(mailbox.reserve_run().is_none(), "当前执行仍持有准入");
    mailbox.enter_idle_sync();

    assert_eq!(mailbox.active_run_ticket(), Some(background));
    let received = inbox.queue_for_test().drain_all_sync();
    assert_eq!(received.len(), 1);
    assert_eq!(
        received[0]
            .message()
            .expect("the fixture entry must contain a message payload")
            .id()
            .as_uuid()
            .to_string(),
        input.input_id
    );
}
