use crate::session::test_resources::mock::MockSessionResources;

use crate::session::user_input_mailbox::window::TERMINAL_CAPACITY;
use crate::session::user_input_mailbox::UserInputAttemptOutcome;
use crate::session::user_input_mailbox::UserInputMailbox;
use crate::session::user_input_mailbox::UserInputQueueError;
use peri_acp_types::identity::{AttemptId, SessionSeq};
use peri_acp_types::messages::BaseMessage;
use peri_acp_types::messages::MessageContent;
use peri_acp_types::messages::MessageId;
use peri_acp_types::session::DispatchUserInputsRequest;
use peri_acp_types::session::EnqueueUserInputRequest;
use peri_acp_types::session::MessageSource;
use peri_acp_types::session::MqCancelOutcome;
use peri_acp_types::session::PluginSelection;
use peri_acp_types::session::SessionInbox;
use peri_acp_types::session::TakeBackUserInputRequest;
use peri_acp_types::session::TurnId;

use peri_acp_types::session::UserInputState;
use peri_acp_types::session::{ExecutionBinding, MessageQueue, MqReceivePurpose};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[path = "user_input_mailbox_terminal_test.rs"]
mod terminal_tests;

#[path = "user_input_mailbox_control_test.rs"]
mod control_tests;

fn make_mailbox() -> (Arc<UserInputMailbox>, Arc<SessionInbox>) {
    let inbox = Arc::new(SessionInbox::new(Arc::new(MessageQueue::new())));
    (
        UserInputMailbox::new("session".into(), inbox.clone(), Arc::new(|_| {})),
        inbox,
    )
}

fn make_input(mailbox: &UserInputMailbox, text: &str) -> EnqueueUserInputRequest {
    EnqueueUserInputRequest {
        session_id: "session".into(),
        generation: mailbox.generation().into(),
        command_id: uuid::Uuid::now_v7().to_string(),
        input_id: uuid::Uuid::now_v7().to_string(),
        content: MessageContent::text(text),
        original_draft: text.into(),
        plugin_selections: Vec::new(),
    }
}

fn make_dispatch(
    mailbox: &UserInputMailbox,
    inputs: &[&EnqueueUserInputRequest],
) -> DispatchUserInputsRequest {
    DispatchUserInputsRequest {
        session_id: "session".into(),
        generation: mailbox.generation().into(),
        command_id: uuid::Uuid::now_v7().to_string(),
        input_ids: inputs.iter().map(|input| input.input_id.clone()).collect(),
    }
}

/// 真实 SessionResources 替身绑定的 mailbox：可注入历史查询失败。
fn make_mock_mailbox(
    resources: Arc<MockSessionResources>,
) -> (Arc<UserInputMailbox>, MessageQueue) {
    let queue = MessageQueue::bind(resources, "thread-x".into());
    let inbox = Arc::new(SessionInbox::new(Arc::new(queue.clone())));
    (
        UserInputMailbox::new("session".into(), inbox, Arc::new(|_| {})),
        queue,
    )
}

fn record_state(mailbox: &UserInputMailbox, input_id: &str) -> Option<(UserInputState, bool)> {
    mailbox
        .state
        .lock()
        .records
        .iter()
        .find(|record| record.input_id == input_id)
        .map(|record| (record.state, record.handed_off))
}

#[test]
fn test_mailbox_running_enqueue_never_wakes_consumption_queue() {
    let (mailbox, inbox) = make_mailbox();
    mailbox
        .attach_external_attempt_sync(CancellationToken::new(), false)
        .expect("the attach external attempt sync fixture result is required for this assertion");
    let input = make_input(&mailbox, "等待下一次 idle");
    let receipt = mailbox
        .enqueue_sync(&input)
        .expect("the enqueue sync fixture result is required for this assertion");
    assert_eq!(
        receipt.results[0].state,
        UserInputState::Queued,
        "运行中输入应保持待发"
    );
    assert!(
        !inbox.queue_for_test().has_wake_up_sync(),
        "普通待办不参与 MQ 唤醒"
    );
    assert!(
        !inbox.queue_for_test().needs_mq_continuation_sync(),
        "普通待办不触发后台空转"
    );
}

#[tokio::test]
async fn test_enter_during_model_request_is_soft_and_stop_wins_replay() {
    let (mailbox, inbox) = make_mailbox();
    let cancel = CancellationToken::new();
    mailbox
        .attach_external_attempt(cancel.clone(), false)
        .await
        .expect("active turn");
    let model_request = mailbox.begin_model_request();
    let input = make_input(&mailbox, "next instructions");
    let receipt = mailbox
        .enqueue_at_tool_boundary(&input)
        .await
        .expect("submit");
    assert_eq!(receipt.results[0].state, UserInputState::Queued);
    assert!(!cancel.is_cancelled());
    assert!(inbox.queue().selected_input_ids().is_empty());
    mailbox
        .enqueue_at_tool_boundary(&input)
        .await
        .expect("same command");
    assert_eq!(mailbox.pending_steer_ids(&[]).len(), 1);
    drop(model_request);
    mailbox.stop().await;
    mailbox
        .enqueue_at_tool_boundary(&input)
        .await
        .expect("replay after stop");
    assert!(mailbox.pending_steer_ids(&[]).is_empty());
    assert_eq!(
        record_state(&mailbox, &input.input_id),
        Some((UserInputState::Queued, false))
    );
}

/// [回归] Enter 所属 attempt 在准入时固定：MQ 入队等待期间换 attempt
/// 不能把输入改绑到后来者的工具边界。
#[tokio::test]
async fn enter_accepted_during_enqueue_does_not_bind_to_a_later_attempt() {
    let resources = MockSessionResources::new();
    let (mailbox, _queue) = make_mock_mailbox(Arc::clone(&resources));
    let first = mailbox
        .attach_external_attempt(CancellationToken::new(), false)
        .await
        .expect("active turn");
    let input = make_input(&mailbox, "绑定提交时的 attempt");
    let input_id = input.input_id.clone();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    *resources
        .mq_enqueue_gate
        .lock()
        .expect("the mq enqueue gate fixture must remain unpoisoned") =
        Some(crate::session::test_resources::mock::ResumeLoadGate {
            entered: entered_tx,
            release: release_rx,
            dropped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });
    let submit_mailbox = Arc::clone(&mailbox);
    let submit = tokio::spawn(async move { submit_mailbox.enqueue_at_tool_boundary(&input).await });
    entered_rx.await.expect("enqueue reaches the write gate");
    mailbox.fail_reserved(&first);
    let second = mailbox
        .attach_external_attempt(CancellationToken::new(), true)
        .await
        .expect("later attempt");
    release_tx.send(()).expect("release enqueue write");
    submit
        .await
        .expect("enqueue task")
        .expect("submit after attempt switch");

    assert_ne!(second, first);
    assert!(
        mailbox.pending_steer_ids(&[]).is_empty(),
        "旧 attempt 的 Enter 不得进入新 attempt 的工具边界"
    );
    assert_eq!(
        record_state(&mailbox, &input_id),
        Some((UserInputState::Queued, false))
    );
}

/// [回归] `NotFound` 不是移交结论：MQ 与历史都没有该 ID 时按 dispatch 口径对账。
#[tokio::test]
async fn send_not_found_removes_the_record_without_claiming_handoff() {
    let resources = MockSessionResources::new();
    let (mailbox, queue) = make_mock_mailbox(Arc::clone(&resources));
    mailbox
        .attach_external_attempt(CancellationToken::new(), false)
        .await
        .expect("active turn");
    let input = make_input(&mailbox, "发送前已缺失");
    mailbox.enqueue(&input).await.expect("enqueue");
    let id = MessageId::from(
        uuid::Uuid::parse_str(&input.input_id).expect("the input id must stay a uuid"),
    );
    assert!(matches!(
        queue
            .cancel_user_input(&id)
            .await
            .expect("cancel removes the queued row"),
        MqCancelOutcome::Cancelled(_)
    ));
    {
        let mut state = mailbox.state.lock();
        crate::session::user_input_mailbox::state::promote_ids(
            &mut state,
            std::slice::from_ref(&input.input_id),
        );
    }

    mailbox
        .send_ready_ids(vec![input.input_id.clone()], true)
        .await;

    assert_eq!(
        record_state(&mailbox, &input.input_id),
        None,
        "MQ 与历史都没有的行必须按对账删除"
    );
    assert!(!mailbox.has_handed_off_inputs());
}

#[test]
fn test_mailbox_idle_input_handoff_is_once_and_keeps_message_identity() {
    let (mailbox, inbox) = make_mailbox();
    let input = make_input(&mailbox, "空闲时立即处理");
    mailbox
        .enqueue_sync(&input)
        .expect("the enqueue sync fixture result is required for this assertion");
    let second = make_input(&mailbox, "预留前的后续输入");
    let receipt = mailbox
        .enqueue_sync(&second)
        .expect("the enqueue sync fixture result is required for this assertion");
    assert_eq!(
        receipt.results[0].state,
        UserInputState::Queued,
        "尚未 reserve 也不能批量释放第二条普通输入"
    );
    let ticket = mailbox
        .reserve_run()
        .expect("the reserve run fixture result is required for this assertion");
    assert!(
        mailbox.snapshot_sync().active_request_id.is_none(),
        "预留阶段还没有客户端执行身份"
    );
    assert!(mailbox.reserve_run().is_none(), "执行预留只能成功一次");
    assert!(
        inbox.queue_for_test().is_empty_sync(),
        "取得运行锁前不能交接"
    );
    assert!(
        mailbox.attach_attempt_sync(&ticket, CancellationToken::new()),
        "预留身份应允许执行"
    );
    assert_eq!(
        mailbox.snapshot_sync().active_request_id.as_deref(),
        Some(ticket.id.as_str()),
        "恢复会话时能取得真实执行身份"
    );
    assert!(
        !mailbox.attach_attempt_sync(&ticket, CancellationToken::new()),
        "相同 ticket 不能启动两次"
    );
    let messages = inbox.queue_for_test().drain_all_sync();
    assert_eq!(messages.len(), 1, "相同输入只交接一次");
    assert_eq!(
        messages[0]
            .message()
            .expect("the fixture entry must contain a message payload")
            .id()
            .as_uuid()
            .to_string(),
        input.input_id,
        "canonical 消息保留准入身份"
    );
}

#[test]
fn test_mailbox_failed_attached_attempt_recovers_unclaimed_and_clears_active_identity() {
    let (mailbox, inbox) = make_mailbox();
    let input = make_input(&mailbox, "装配失败");
    mailbox
        .enqueue_sync(&input)
        .expect("the enqueue sync fixture result is required for this assertion");
    let ticket = mailbox
        .reserve_run()
        .expect("the reserve run fixture result is required for this assertion");
    let cancel = CancellationToken::new();
    mailbox.attach_attempt_sync(&ticket, cancel.clone());
    assert!(
        mailbox.run_started_event(&ticket).is_some(),
        "已 attach 身份可建立客户端 lease"
    );
    mailbox.fail_reserved(&ticket);
    assert!(cancel.is_cancelled(), "早退清理取消真实 token");
    assert!(
        inbox.queue_for_test().is_empty_sync(),
        "实际未领取项必须从 MQ 撤出"
    );
    assert_eq!(
        mailbox.snapshot_sync().items[0].state,
        UserInputState::Queued,
        "早退保留可编辑待办"
    );
    assert!(
        mailbox.snapshot_sync().active_request_id.is_none(),
        "早退清理执行投影"
    );
    assert!(
        mailbox.run_started_event(&ticket).is_none(),
        "旧 ticket 不能重新建立客户端 lease"
    );
}

#[test]
fn test_mailbox_single_dispatch_preserves_ordinary_queue_until_natural_completion() {
    let (mailbox, inbox) = make_mailbox();
    let cancel = CancellationToken::new();
    let first = mailbox
        .attach_external_attempt_sync(cancel.clone(), false)
        .expect("the attach external attempt sync fixture result is required for this assertion");
    let a = make_input(&mailbox, "A");
    let b = make_input(&mailbox, "B");
    let c = make_input(&mailbox, "C");
    for input in [&a, &b, &c] {
        mailbox
            .enqueue_sync(input)
            .expect("the enqueue sync fixture result is required for this assertion");
    }
    let steer_gate = mailbox.begin_model_request();
    mailbox
        .dispatch_sync(&make_dispatch(&mailbox, &[&b]))
        .expect("the dispatch sync fixture result is required for this assertion");
    assert!(
        cancel.is_cancelled(),
        "模型请求在途时的单发请求中断真实执行"
    );
    assert!(
        mailbox.is_steer_interruption(&first),
        "流式 steer 必须保留可区分的取消原因"
    );
    drop(steer_gate);
    assert!(
        !mailbox.state.lock().model_request_in_flight,
        "guard 结束必须关闭模型请求窗口"
    );
    assert_eq!(
        mailbox
            .snapshot_sync()
            .items
            .iter()
            .find(|item| item.input_id == b.input_id)
            .map(|item| item.state),
        Some(UserInputState::Dispatching),
        "发送选择已提交，但 Receive 尚未领取"
    );
    mailbox.finish_attempt_sync(&first, UserInputAttemptOutcome::Interrupted);
    let next = mailbox
        .reserve_run()
        .expect("the reserve run fixture result is required for this assertion");
    mailbox.attach_attempt_sync(&next, CancellationToken::new());
    let received = inbox.queue_for_test().drain_all_sync();
    assert_eq!(
        received
            .iter()
            .map(|message| message
                .message()
                .expect("the fixture entry must contain a message payload")
                .content())
            .collect::<Vec<_>>(),
        ["B"],
        "单发只交接选中 B"
    );
    let ids: Vec<_> = received
        .iter()
        .map(|message| {
            message
                .message()
                .expect("the fixture entry must contain a message payload")
                .id()
        })
        .collect();
    mailbox.mark_claimed_sync(&ids);
    mailbox.mark_delivered_sync(&ids);
    assert_eq!(
        mailbox
            .snapshot_sync()
            .items
            .iter()
            .map(|item| item.state)
            .collect::<Vec<_>>(),
        [UserInputState::Queued, UserInputState::Queued],
        "内部中断不能释放 A/C"
    );
    mailbox.record_attempt_outcome(&next, UserInputAttemptOutcome::Completed);
    mailbox.finish_attempt_sync(&next, UserInputAttemptOutcome::Failed);
    let last = mailbox
        .reserve_run()
        .expect("the reserve run fixture result is required for this assertion");
    mailbox.attach_attempt_sync(&last, CancellationToken::new());
    let received = inbox.queue_for_test().drain_all_sync();
    assert_eq!(
        received
            .iter()
            .map(|message| message
                .message()
                .expect("the fixture entry must contain a message payload")
                .content())
            .collect::<Vec<_>>(),
        ["A"],
        "自然成功只释放第一条普通待办"
    );
    let id = received[0]
        .message()
        .expect("the fixture entry must contain a message payload")
        .id();
    mailbox.mark_claimed_sync(&[id]);
    mailbox.mark_delivered_sync(&[id]);
    assert_eq!(
        mailbox.snapshot_sync().items[0].state,
        UserInputState::Queued
    );
    mailbox.finish_attempt_sync(&last, UserInputAttemptOutcome::Completed);
    let next = mailbox
        .reserve_run()
        .expect("the reserve run fixture result is required for this assertion");
    mailbox.attach_attempt_sync(&next, CancellationToken::new());
    let received = inbox.queue_for_test().drain_all_sync();
    assert_eq!(received.len(), 1);
    assert_eq!(
        received[0]
            .message()
            .expect("the fixture entry must contain a message payload")
            .content(),
        "C",
        "A 完成后才自动发送 C"
    );
}

#[test]
fn test_mailbox_dispatch_during_model_request_cancels_and_records_steer() {
    let (mailbox, _) = make_mailbox();
    let cancel = CancellationToken::new();
    let ticket = mailbox
        .attach_external_attempt_sync(cancel.clone(), false)
        .expect("the attach external attempt sync fixture result is required for this assertion");
    let input = make_input(&mailbox, "流式期间的立即发送");
    mailbox
        .enqueue_sync(&input)
        .expect("the enqueue sync fixture result is required for this assertion");

    let gate = mailbox.begin_model_request();
    mailbox
        .dispatch_sync(&make_dispatch(&mailbox, &[&input]))
        .expect("the dispatch sync fixture result is required for this assertion");

    assert!(cancel.is_cancelled(), "模型请求在途时必须硬取消当前执行");
    assert!(
        mailbox.is_steer_interruption(&ticket),
        "流式 steer 必须与 Stop 区分"
    );
    drop(gate);
    assert!(
        !mailbox.state.lock().model_request_in_flight,
        "guard Drop 必须关闭模型请求窗口"
    );
}

#[test]
fn test_mailbox_dispatch_outside_model_request_hands_off_without_cancel() {
    let (mailbox, inbox) = make_mailbox();
    let cancel = CancellationToken::new();
    let ticket = mailbox
        .attach_external_attempt_sync(cancel.clone(), false)
        .expect("the attach external attempt sync fixture result is required for this assertion");
    let input = make_input(&mailbox, "工具批次中的立即发送");
    mailbox
        .enqueue_sync(&input)
        .expect("the enqueue sync fixture result is required for this assertion");

    mailbox
        .dispatch_sync(&make_dispatch(&mailbox, &[&input]))
        .expect("the dispatch sync fixture result is required for this assertion");

    assert!(!cancel.is_cancelled(), "非模型窗口不得取消当前执行");
    assert!(!mailbox.is_steer_interruption(&ticket));
    assert!(
        mailbox
            .state
            .lock()
            .active
            .as_ref()
            .is_some_and(|active| active.reason
                == crate::session::user_input_mailbox::mailbox::InterruptReason::None),
        "软注入不得设置 steer 取消原因"
    );
    let released = inbox.queue_for_test().drain_all_sync();
    assert_eq!(released.len(), 1, "MQ 仍须放行以便下一次 Receive 摄取");
    assert_eq!(
        released[0]
            .message()
            .expect("the fixture entry must contain a message payload")
            .id()
            .as_uuid()
            .to_string(),
        input.input_id
    );
}

#[test]
fn test_mailbox_stop_during_model_request_cancels_immediately() {
    let (mailbox, _) = make_mailbox();
    let cancel = CancellationToken::new();
    mailbox
        .attach_external_attempt_sync(cancel.clone(), false)
        .expect("the attach external attempt sync fixture result is required for this assertion");
    let gate = mailbox.begin_model_request();

    mailbox.stop_sync();

    assert!(cancel.is_cancelled(), "Stop 不受阶段门限制");
    assert!(
        !mailbox.state.lock().model_request_in_flight,
        "Stop 必须复位防御性阶段标志"
    );
    drop(gate);
}

#[test]
fn test_mailbox_stop_outside_model_request_cancels_immediately() {
    let (mailbox, _) = make_mailbox();
    let cancel = CancellationToken::new();
    mailbox
        .attach_external_attempt_sync(cancel.clone(), false)
        .expect("the attach external attempt sync fixture result is required for this assertion");

    mailbox.stop_sync();

    assert!(cancel.is_cancelled(), "Stop 在任何阶段都立即取消");
}

#[test]
fn test_attach_attempt_clears_model_request_flag() {
    let (mailbox, _) = make_mailbox();
    let input = make_input(&mailbox, "旧请求泄漏");
    mailbox
        .enqueue_sync(&input)
        .expect("the enqueue sync fixture result is required for this assertion");
    let ticket = mailbox
        .reserve_run()
        .expect("the reserve run fixture result is required for this assertion");
    let gate = mailbox.begin_model_request();
    assert!(mailbox.state.lock().model_request_in_flight);

    assert!(mailbox.attach_attempt_sync(&ticket, CancellationToken::new()));
    assert!(
        !mailbox.state.lock().model_request_in_flight,
        "attach_attempt 必须防止旧窗口泄漏到新 attempt"
    );
    drop(gate);
}

#[test]
fn test_finish_attempt_clears_model_request_flag() {
    let (mailbox, _) = make_mailbox();
    let ticket = mailbox
        .attach_external_attempt_sync(CancellationToken::new(), false)
        .expect("the attach external attempt sync fixture result is required for this assertion");
    let gate = mailbox.begin_model_request();
    assert!(mailbox.state.lock().model_request_in_flight);

    mailbox.finish_attempt_sync(&ticket, UserInputAttemptOutcome::Completed);

    assert!(
        !mailbox.state.lock().model_request_in_flight,
        "finish_attempt 必须防止旧窗口泄漏到下一个 attempt"
    );
    drop(gate);
}

#[test]
fn test_fail_reserved_clears_model_request_flag() {
    let (mailbox, _) = make_mailbox();
    let input = make_input(&mailbox, "早退");
    mailbox
        .enqueue_sync(&input)
        .expect("the enqueue sync fixture result is required for this assertion");
    let ticket = mailbox
        .reserve_run()
        .expect("the reserve run fixture result is required for this assertion");
    mailbox.attach_attempt_sync(&ticket, CancellationToken::new());
    let gate = mailbox.begin_model_request();

    mailbox.fail_reserved(&ticket);

    assert!(
        !mailbox.state.lock().model_request_in_flight,
        "fail_reserved 的 Drop 路径必须运行期复位"
    );
    drop(gate);
}

#[test]
fn test_mailbox_soft_dispatch_then_failed_attempt_returns_input_to_queued() {
    let (mailbox, inbox) = make_mailbox();
    let cancel = CancellationToken::new();
    let ticket = mailbox
        .attach_external_attempt_sync(cancel.clone(), false)
        .expect("the attach external attempt sync fixture result is required for this assertion");
    let input = make_input(&mailbox, "失败前未摄取");
    mailbox
        .enqueue_sync(&input)
        .expect("the enqueue sync fixture result is required for this assertion");
    mailbox
        .dispatch_sync(&make_dispatch(&mailbox, &[&input]))
        .expect("the dispatch sync fixture result is required for this assertion");
    assert!(!cancel.is_cancelled());
    assert_eq!(
        mailbox.snapshot_sync().items[0].state,
        UserInputState::Dispatching
    );

    mailbox.finish_attempt_sync(&ticket, UserInputAttemptOutcome::Failed);

    assert_eq!(
        mailbox.snapshot_sync().items[0].state,
        UserInputState::Queued,
        "摄取前失败必须回到待发，等待用户重新发送"
    );
    assert!(
        inbox.queue_for_test().is_empty_sync(),
        "未摄取输入必须从 MQ 撤出"
    );
    let input_id = MessageId::from(
        uuid::Uuid::parse_str(&input.input_id)
            .expect("the fixture input id must be a valid UUID for this assertion"),
    );
    assert!(
        mailbox.mark_delivered_sync(&[input_id]).is_empty(),
        "摄取前失败不得写交付历史"
    );
}

#[test]
fn test_mailbox_soft_dispatch_then_stop_returns_input_to_queued() {
    let (mailbox, inbox) = make_mailbox();
    let cancel = CancellationToken::new();
    mailbox
        .attach_external_attempt_sync(cancel.clone(), false)
        .expect("the attach external attempt sync fixture result is required for this assertion");
    let input = make_input(&mailbox, "Stop 前未摄取");
    mailbox
        .enqueue_sync(&input)
        .expect("the enqueue sync fixture result is required for this assertion");
    mailbox
        .dispatch_sync(&make_dispatch(&mailbox, &[&input]))
        .expect("the dispatch sync fixture result is required for this assertion");

    mailbox.stop_sync();

    assert!(cancel.is_cancelled(), "Stop 立即取消");
    assert_eq!(
        mailbox.snapshot_sync().items[0].state,
        UserInputState::Queued,
        "Stop 后未摄取输入回到待发"
    );
    assert!(
        inbox.queue_for_test().is_empty_sync(),
        "Stop 必须撤出未领取的发送选择"
    );
}

#[test]
fn test_mailbox_dispatch_snapshot_and_consecutive_commands_preserve_acceptance_order() {
    let (mailbox, inbox) = make_mailbox();
    let current = mailbox
        .attach_external_attempt_sync(CancellationToken::new(), false)
        .expect("the attach external attempt sync fixture result is required for this assertion");
    let inputs: Vec<_> = ["A", "B", "C", "D"]
        .into_iter()
        .map(|text| make_input(&mailbox, text))
        .collect();
    for input in &inputs[..3] {
        mailbox
            .enqueue_sync(input)
            .expect("the enqueue sync fixture result is required for this assertion");
    }
    // 本用例验证发送选择的接受顺序；显式持有模型请求窗口，保持“立即发送
    // 打断旧执行”的前置条件（窗口外的发送只移交 MQ，不取消）。
    let gate = mailbox.begin_model_request();
    mailbox
        .dispatch_sync(&make_dispatch(&mailbox, &[&inputs[1]]))
        .expect("the dispatch sync fixture result is required for this assertion");
    mailbox
        .dispatch_sync(&make_dispatch(
            &mailbox,
            &[&inputs[2], &inputs[0], &inputs[1]],
        ))
        .expect("the dispatch sync fixture result is required for this assertion");
    drop(gate);
    mailbox
        .enqueue_sync(&inputs[3])
        .expect("the enqueue sync fixture result is required for this assertion");
    mailbox.finish_attempt_sync(&current, UserInputAttemptOutcome::Interrupted);
    let next = mailbox
        .reserve_run()
        .expect("the reserve run fixture result is required for this assertion");
    mailbox.attach_attempt_sync(&next, CancellationToken::new());
    assert_eq!(
        inbox
            .queue_for_test()
            .drain_all_sync()
            .iter()
            .map(|message| message
                .message()
                .expect("the fixture entry must contain a message payload")
                .content())
            .collect::<Vec<_>>(),
        ["A", "B", "C"],
        "Receive 按服务端 (enqueued_at,id) 排序，发送选择不另建排序"
    );
    assert_eq!(
        mailbox
            .snapshot_sync()
            .items
            .last()
            .expect("mailbox snapshot sync fixture must contain the expected element")
            .state,
        UserInputState::Queued,
        "快照点击后新 D 不加入 ALL"
    );
}

/// [回归测试] Delivered / Withdrawn 是可查询的终态，不能只从 pending 列表消失；
/// 重复确认必须幂等。
#[test]
fn test_mailbox_snapshot_exposes_terminal_states_and_deduplicates_delivery() {
    let (mailbox, inbox) = make_mailbox();
    mailbox
        .attach_external_attempt_sync(CancellationToken::new(), false)
        .expect("the attach external attempt sync fixture result is required for this assertion");
    let delivered_input = make_input(&mailbox, "已交付");
    mailbox
        .enqueue_sync(&delivered_input)
        .expect("the enqueue sync fixture result is required for this assertion");
    mailbox.enter_idle_sync();
    let messages = inbox.queue_for_test().drain_all_sync();
    let delivered_id = messages[0]
        .message()
        .expect("the fixture entry must contain a message payload")
        .id();
    mailbox.mark_claimed_sync(&[delivered_id]);
    assert_eq!(mailbox.mark_delivered_sync(&[delivered_id]).len(), 1);
    assert!(
        mailbox.mark_delivered_sync(&[delivered_id]).is_empty(),
        "重复交付确认不得重复通知"
    );

    let withdrawn_input = make_input(&mailbox, "已撤回");
    mailbox
        .enqueue_sync(&withdrawn_input)
        .expect("the enqueue sync fixture result is required for this assertion");
    mailbox
        .take_back_sync(&TakeBackUserInputRequest {
            session_id: "session".into(),
            generation: mailbox.generation().into(),
            command_id: "withdraw-terminal".into(),
            input_id: withdrawn_input.input_id.clone(),
        })
        .expect("the take back sync fixture result is required for this assertion");

    let snapshot = mailbox.snapshot_sync();
    assert!(snapshot.items.is_empty(), "终态不应继续出现在待发列表");
    assert!(snapshot.terminal_items.iter().any(|item| {
        item.input_id == delivered_input.input_id && item.state == UserInputState::Delivered
    }));
    assert!(snapshot.terminal_items.iter().any(|item| {
        item.input_id == withdrawn_input.input_id && item.state == UserInputState::Withdrawn
    }));
}

/// [回归测试] 交付日志条目在发布时获得事件序号；重复绑定保留首个序号。
#[test]
fn test_bind_delivery_sequence_orders_journal_and_is_idempotent() {
    let (mailbox, inbox) = make_mailbox();
    mailbox
        .attach_external_attempt_sync(CancellationToken::new(), false)
        .expect("the attach external attempt sync fixture result is required for this assertion");
    let input = make_input(&mailbox, "带序号交付");
    mailbox
        .enqueue_sync(&input)
        .expect("the enqueue sync fixture result is required for this assertion");
    mailbox.enter_idle_sync();
    let messages = inbox.queue_for_test().drain_all_sync();
    let id = messages[0]
        .message()
        .expect("the fixture entry must contain a message payload")
        .id();
    mailbox.mark_claimed_sync(&[id]);
    mailbox.mark_delivered_sync(&[id]);

    let pending = mailbox.pending_delivery_notifications();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].session_seq, None,
        "mark_delivered 时尚未发布事件，序号必须显式为 None"
    );

    let first = SessionSeq::initial().next();
    mailbox.bind_delivery_sequence(&input.input_id, first);
    assert_eq!(
        mailbox.pending_delivery_notifications()[0].session_seq,
        Some(first)
    );

    mailbox.bind_delivery_sequence(&input.input_id, first.next());
    assert_eq!(
        mailbox.pending_delivery_notifications()[0].session_seq,
        Some(first),
        "重复绑定不得覆盖首个序号"
    );
}

include!("user_input_mailbox_test_section_2.rs");
