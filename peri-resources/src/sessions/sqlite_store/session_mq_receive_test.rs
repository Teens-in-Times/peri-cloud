/// 取消与 Receive 竞争：串行化下只有一个结果成立，且不会出现「删了待发项却报告已发送」。
#[tokio::test]
async fn cancel_and_receive_have_exactly_one_winner() {
    let fixture = Fixture::new().await;
    let thread = fixture.thread("mq").await;
    let id = MessageId::new();
    let entry = fixture
        .data
        .mq_enqueue(&thread, input(id, "race"))
        .await
        .unwrap();
    let (cancelled, received) = tokio::join!(
        fixture.data.mq_cancel(&thread, id),
        fixture.data.mq_receive(
            &thread,
            selection(&entry.entry, MqReceiveDisposition::Commit)
        ),
    );
    let cancel = cancelled.unwrap();
    let receive = received.unwrap();
    let committed = receive.items[0].status == MqReceiveStatus::Committed;
    let cancelled_now = matches!(cancel, MqCancelOutcome::Cancelled(_));
    assert!(
        committed ^ cancelled_now,
        "只有一个结果成立：cancel={cancel:?} receive={:?}",
        receive.items[0].status
    );
    if committed {
        assert!(matches!(cancel, MqCancelOutcome::AlreadyHandedOff));
        assert_eq!(history_len(&fixture, &thread).await, 1);
    } else {
        assert_eq!(receive.items[0].status, MqReceiveStatus::Missing);
        assert_eq!(history_len(&fixture, &thread).await, 0);
    }
    assert_eq!(mq_count(&fixture, &thread).await, 0);
}

/// 历史已有同 ID 不同内容：Conflict 删 MQ 行、绝不写历史（整批仍是提交，但历史不动）。
#[tokio::test]
async fn receive_conflict_removes_the_pending_row_without_writing_history() {
    let fixture = Fixture::new().await;
    let thread = fixture.thread("mq").await;
    let id = MessageId::new();
    let entry = fixture
        .data
        .mq_enqueue(&thread, input(id, "pending"))
        .await
        .unwrap();
    let history = PersistedPayload::Message(human(id, "already different"));
    fixture
        .data
        .append_history(&thread, &[history])
        .await
        .unwrap();

    let outcome = fixture
        .data
        .mq_receive(
            &thread,
            selection(&entry.entry, MqReceiveDisposition::Commit),
        )
        .await
        .unwrap();
    assert_eq!(outcome.items[0].status, MqReceiveStatus::Conflict);
    assert_eq!(mq_count(&fixture, &thread).await, 0);
    let (content,): (String,) =
        sqlx::query_as("SELECT content FROM messages WHERE thread_id = ?1 AND message_id = ?2")
            .bind(thread.as_str())
            .bind(id.as_uuid().to_string())
            .fetch_one(&fixture.store.database.pool)
            .await
            .unwrap();
    assert!(
        content.contains("already different"),
        "历史不能被待发内容覆盖: {content}"
    );
}

/// 同 ID 同内容已在历史：AlreadyPresent，只删 MQ 行。
#[tokio::test]
async fn receive_reports_already_present_for_the_same_content() {
    let fixture = Fixture::new().await;
    let thread = fixture.thread("mq").await;
    let id = MessageId::new();
    let requested = input(id, "same content");
    let entry = fixture
        .data
        .mq_enqueue(&thread, requested.clone())
        .await
        .unwrap();
    fixture
        .data
        .append_history(&thread, &[requested.payload])
        .await
        .unwrap();
    let outcome = fixture
        .data
        .mq_receive(
            &thread,
            selection(&entry.entry, MqReceiveDisposition::Commit),
        )
        .await
        .unwrap();
    assert_eq!(outcome.items[0].status, MqReceiveStatus::AlreadyPresent);
    assert_eq!(history_len(&fixture, &thread).await, 1);
    assert_eq!(mq_count(&fixture, &thread).await, 0);
}

/// 选择基于旧快照（水位不符）是 Missing：什么都不写。
#[tokio::test]
async fn receive_missing_selection_writes_nothing() {
    let fixture = Fixture::new().await;
    let thread = fixture.thread("mq").await;
    let id = MessageId::new();
    let entry = fixture
        .data
        .mq_enqueue(&thread, input(id, "pending"))
        .await
        .unwrap();
    let mut stale = entry.entry.clone();
    stale.enqueued_at = "2000-01-01T00:00:00.000000Z".to_owned();
    let outcome = fixture
        .data
        .mq_receive(&thread, selection(&stale, MqReceiveDisposition::Commit))
        .await
        .unwrap();
    assert_eq!(outcome.items[0].status, MqReceiveStatus::Missing);
    assert_eq!(history_len(&fixture, &thread).await, 0);
    assert_eq!(
        mq_count(&fixture, &thread).await,
        1,
        "不匹配的行必须原样保留"
    );
}

/// 批次内失败必须整批回滚：损坏行让 Receive 失败时，历史与其它 MQ 行都不动。
#[tokio::test]
async fn receive_rolls_back_the_whole_batch_on_a_corrupt_row() {
    let fixture = Fixture::new().await;
    let thread = fixture.thread("mq").await;
    let good = fixture
        .data
        .mq_enqueue(&thread, input(MessageId::new(), "good"))
        .await
        .unwrap();
    let corrupt = MessageId::new();
    sqlx::query(
        "INSERT INTO thread_mq (thread_id, id, enqueued_at, kind, source, policy_json,
            payload_json, original_draft) VALUES (?1, ?2, ?3, 'prompt', 'user_input', ?4, ?5, NULL)",
    )
    .bind(thread.as_str())
    .bind(corrupt.as_uuid().to_string())
    .bind("2026-11-01T00:00:00.000000Z")
    .bind(serde_json::to_string(&MessagePolicy::ensure_processing()).unwrap())
    .bind("not a persisted payload")
    .execute(&fixture.store.database.pool)
    .await
    .unwrap();
    let broken = peri_acp_types::session::MqEntry {
        id: corrupt,
        enqueued_at: "2026-11-01T00:00:00.000000Z".to_owned(),
        kind: MessageKind::Prompt,
        source: MessageSource::UserInput,
        payload: good.entry.payload.clone(),
        policy: MessagePolicy::ensure_processing(),
        original_draft: None,
    };
    let selection = MqReceiveSelection {
        items: vec![
            MqReceiveItem {
                entry: good.entry.clone(),
                disposition: MqReceiveDisposition::Commit,
            },
            MqReceiveItem {
                entry: broken,
                disposition: MqReceiveDisposition::Commit,
            },
        ],
        purpose: MqReceivePurpose::ModelLoop,
        execution: selection(&good.entry, MqReceiveDisposition::Commit).execution,
    };
    let error = fixture
        .data
        .mq_receive(&thread, selection)
        .await
        .unwrap_err();
    assert!(
        matches!(error.kind(), SessionResourceErrorKind::Corrupt { .. }),
        "{error:?}"
    );
    assert_eq!(history_len(&fixture, &thread).await, 0, "前一项也不得写入");
    assert_eq!(mq_count(&fixture, &thread).await, 2, "两个待发行都原样保留");
}

/// Suppress 只删 MQ 行，不写历史。
#[tokio::test]
async fn receive_suppress_removes_only_the_pending_row() {
    let fixture = Fixture::new().await;
    let thread = fixture.thread("mq").await;
    let entry = fixture
        .data
        .mq_enqueue(
            &thread,
            MqInput {
                kind: MessageKind::Info,
                source: MessageSource::HookStopIntent,
                original_draft: None,
                ..input(MessageId::new(), "stale control")
            },
        )
        .await
        .unwrap();
    let outcome = fixture
        .data
        .mq_receive(
            &thread,
            selection(&entry.entry, MqReceiveDisposition::Suppress),
        )
        .await
        .unwrap();
    assert_eq!(outcome.items[0].status, MqReceiveStatus::Suppressed);
    assert_eq!(history_len(&fixture, &thread).await, 0);
    assert_eq!(mq_count(&fixture, &thread).await, 0);
}

/// 移交同时维护计数与首条用户消息标题。
#[tokio::test]
async fn receive_maintains_count_and_derived_title() {
    let fixture = Fixture::new().await;
    let thread = fixture.thread("mq").await;
    let entry = fixture
        .data
        .mq_enqueue(&thread, input(MessageId::new(), "derived title"))
        .await
        .unwrap();
    fixture
        .data
        .mq_receive(
            &thread,
            selection(&entry.entry, MqReceiveDisposition::Commit),
        )
        .await
        .unwrap();
    let (title, count): (Option<String>, i64) =
        sqlx::query_as("SELECT title, message_count FROM threads WHERE id = ?1")
            .bind(thread.as_str())
            .fetch_one(&fixture.store.database.pool)
            .await
            .unwrap();
    assert_eq!(title.as_deref(), Some("derived title"));
    assert_eq!(count, 1);
}

/// 提交前重开：MQ 行在、历史不在；移交后重开：历史在、MQ 不在。
#[tokio::test]
async fn reopen_on_both_sides_of_the_receive_commit() {
    let fixture = Fixture::new().await;
    let thread = fixture.thread("mq").await;
    let entry = fixture
        .data
        .mq_enqueue(&thread, input(MessageId::new(), "survives"))
        .await
        .unwrap();
    let path = fixture.path();
    fixture.store.close().await;
    let reopened = SqliteThreadStore::new(&path).await.unwrap();
    let data = SqliteSessionData::new(Arc::clone(&reopened.database));
    assert_eq!(
        data.mq_query(&thread, MqFilter::default())
            .await
            .unwrap()
            .len(),
        1,
        "提交前重开：MQ 行还在"
    );
    let history: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE thread_id = ?1")
        .bind(thread.as_str())
        .fetch_one(&reopened.database.pool)
        .await
        .unwrap();
    assert_eq!(history, 0, "提交前重开：历史不存在");

    let received = data
        .mq_receive(
            &thread,
            selection(&entry.entry, MqReceiveDisposition::Commit),
        )
        .await
        .unwrap();
    assert_eq!(received.items[0].status, MqReceiveStatus::Committed);
    reopened.close().await;
    let third = SqliteThreadStore::new(&path).await.unwrap();
    let data = SqliteSessionData::new(Arc::clone(&third.database));
    assert!(data
        .mq_query(&thread, MqFilter::default())
        .await
        .unwrap()
        .is_empty());
    let history: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE thread_id = ?1")
        .bind(thread.as_str())
        .fetch_one(&third.database.pool)
        .await
        .unwrap();
    assert_eq!(history, 1);
    third.close().await;
}

/// rewind 与 mq_clear 都清理该 thread 的待发行，且不碰历史保留前缀。
#[tokio::test]
async fn rewind_and_clear_drop_pending_rows() {
    let fixture = Fixture::new().await;
    let thread = fixture.thread("mq").await;
    let first = fixture
        .data
        .mq_enqueue(&thread, input(MessageId::new(), "first"))
        .await
        .unwrap();
    fixture
        .data
        .mq_receive(
            &thread,
            selection(&first.entry, MqReceiveDisposition::Commit),
        )
        .await
        .unwrap();
    fixture
        .data
        .mq_enqueue(&thread, input(MessageId::new(), "pending"))
        .await
        .unwrap();
    let outcome = fixture
        .data
        .rewind_history(&thread, RewindBoundary::KeepThrough(first.entry.id), &[])
        .await
        .unwrap();
    assert_eq!(
        outcome,
        peri_acp_types::session_resources::RewindOutcome::Applied
    );
    assert_eq!(mq_count(&fixture, &thread).await, 0, "rewind 清空待发行");
    assert_eq!(history_len(&fixture, &thread).await, 1, "保留前缀不动");

    fixture
        .data
        .mq_enqueue(&thread, input(MessageId::new(), "again"))
        .await
        .unwrap();
    fixture.data.mq_clear(&thread).await.unwrap();
    assert_eq!(mq_count(&fixture, &thread).await, 0);
}

/// thread 删除清理 MQ（显式删除，不依赖级联）。
#[tokio::test]
async fn deleting_the_thread_drops_pending_rows() {
    let fixture = Fixture::new().await;
    let thread = fixture.thread("mq").await;
    let id = MessageId::new();
    fixture
        .data
        .mq_enqueue(&thread, input(id, "pending"))
        .await
        .unwrap();
    fixture.data.delete_tree(&thread).await.unwrap();
    let (rows,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM thread_mq WHERE id = ?1")
        .bind(id.as_uuid().to_string())
        .fetch_one(&fixture.store.database.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

// ─── 终态结算：终态 + 工作意图 + 结果接纳一次提交 ──────────────────────────────

/// 读 `threads` 行的 `(agent_status, work_state)`，用于断言「一条都没写」。
async fn terminal_facts(fixture: &Fixture, thread: &ThreadId) -> (String, Option<String>) {
    sqlx::query_as("SELECT agent_status, work_state FROM threads WHERE id = ?1")
        .bind(thread.as_str())
        .fetch_one(&fixture.store.database.pool)
        .await
        .unwrap()
}

fn reminder(body: &str) -> peri_acp_types::system_reminder::TrustedSystemReminder {
    use peri_acp_types::system_reminder::{
        ReminderAudience, ReminderAudiences, ReminderCategory, ReminderDelivery, ReminderSeverity,
        ReminderSource, SystemReminder, TrustedSystemReminderFactory, SYSTEM_REMINDER_VERSION,
    };
    TrustedSystemReminderFactory::for_producer()
        .construct(SystemReminder {
            version: SYSTEM_REMINDER_VERSION,
            category: ReminderCategory::Task,
            source: ReminderSource("task_manager".into()),
            kind: "terminal".into(),
            severity: ReminderSeverity::Info,
            delivery: ReminderDelivery::Required,
            audiences: ReminderAudiences(vec![ReminderAudience::Model]),
            body: body.into(),
            summary: None,
            metadata: serde_json::json!({}),
        })
        .unwrap()
}

fn terminal_input(id: MessageId, body: &str) -> MqInput {
    MqInput {
        id,
        kind: MessageKind::Defer,
        source: MessageSource::SubAgentComplete,
        payload: PersistedPayload::SystemReminder {
            id,
            reminder: reminder(body),
        },
        policy: MessagePolicy::ensure_processing(),
        original_draft: None,
    }
}

/// 无结果终态：一次条件更新同时写终态与工作意图。
#[tokio::test]
async fn settle_terminal_without_result_commits_status_and_work_state() {
    use peri_acp_types::thread::{AgentStatus, WorkState};

    let fixture = Fixture::new().await;
    let child = fixture.thread("child").await;
    fixture
        .data
        .set_work_state(&child, WorkState::Working)
        .await
        .unwrap();

    let outcome = fixture
        .data
        .settle_terminal_without_result(&child, AgentStatus::Done, WorkState::Idle)
        .await
        .unwrap();
    assert_eq!(outcome, TerminalSettlementOutcome::Settled);
    assert_eq!(
        terminal_facts(&fixture, &child).await,
        ("done".to_owned(), Some("idle".to_owned()))
    );
}

/// 重复结算读取既有终态并返回 no-op，不改写工作意图。
#[tokio::test]
async fn settle_terminal_without_result_is_idempotent() {
    use peri_acp_types::thread::{AgentStatus, WorkState};

    let fixture = Fixture::new().await;
    let child = fixture.thread("child").await;
    fixture
        .data
        .settle_terminal_without_result(&child, AgentStatus::Done, WorkState::Idle)
        .await
        .unwrap();
    let replay = fixture
        .data
        .settle_terminal_without_result(&child, AgentStatus::Done, WorkState::Working)
        .await
        .unwrap();
    assert_eq!(
        replay,
        TerminalSettlementOutcome::AlreadyTerminal(AgentStatus::Done)
    );
    assert_eq!(
        terminal_facts(&fixture, &child).await,
        ("done".to_owned(), Some("idle".to_owned())),
        "no-op 不得改写 work_state"
    );
}

/// child 不存在时按 NotFound 失败，不创建行。
#[tokio::test]
async fn settle_terminal_without_result_rejects_unknown_child() {
    use peri_acp_types::thread::{AgentStatus, WorkState};

    let fixture = Fixture::new().await;
    let child = ThreadId::from("missing-child".to_owned());
    let error = fixture
        .data
        .settle_terminal_without_result(&child, AgentStatus::Error, WorkState::Idle)
        .await
        .unwrap_err();
    assert!(matches!(error.kind(), SessionResourceErrorKind::NotFound));
}

/// 后到的终态不能覆盖先提交的终态。
#[tokio::test]
async fn settle_terminal_without_result_does_not_overwrite_terminal() {
    use peri_acp_types::thread::{AgentStatus, WorkState};

    let fixture = Fixture::new().await;
    let child = fixture.thread("child").await;
    fixture
        .data
        .settle_terminal_without_result(&child, AgentStatus::Done, WorkState::Idle)
        .await
        .unwrap();
    let late = fixture
        .data
        .settle_terminal_without_result(&child, AgentStatus::Error, WorkState::Idle)
        .await
        .unwrap();
    assert_eq!(
        late,
        TerminalSettlementOutcome::AlreadyTerminal(AgentStatus::Done)
    );
    assert_eq!(
        terminal_facts(&fixture, &child).await,
        ("done".to_owned(), Some("idle".to_owned()))
    );
}

/// 三步在同一提交里生效：重开后终态、工作意图与目标 MQ 行都能读到。
#[tokio::test]
async fn finalize_child_terminal_commits_status_work_state_and_result() {
    use peri_acp_types::thread::{AgentStatus, WorkState};

    let fixture = Fixture::new().await;
    let child = fixture.thread("child").await;
    let target = fixture.thread("parent").await;
    fixture
        .data
        .set_work_state(&child, WorkState::Working)
        .await
        .unwrap();
    let id = MessageId::new();
    let outcome = fixture
        .data
        .finalize_child_terminal(
            &child,
            AgentStatus::Done,
            WorkState::Idle,
            &target,
            terminal_input(id, "child done"),
        )
        .await
        .unwrap();
    assert_eq!(outcome.disposition, MqEnqueueDisposition::Inserted);
    assert_eq!(
        terminal_facts(&fixture, &child).await,
        ("done".to_owned(), Some("idle".to_owned()))
    );
    assert_eq!(mq_count(&fixture, &target).await, 1);
    assert_eq!(mq_count(&fixture, &child).await, 0);

    // 重试同一身份只复用原接纳结果，不产生第二行。
    let replay = fixture
        .data
        .finalize_child_terminal(
            &child,
            AgentStatus::Done,
            WorkState::Idle,
            &target,
            terminal_input(id, "child done"),
        )
        .await
        .unwrap();
    assert_eq!(replay.disposition, MqEnqueueDisposition::Existing);
    assert_eq!(mq_count(&fixture, &target).await, 1);
}

/// 结果接纳与接收方工作意图共用一次提交：EnsureProcessing 提升，passive 不授予。
#[tokio::test]
async fn finalize_child_terminal_maintains_receiver_work_intent() {
    use peri_acp_types::thread::{AgentStatus, WorkState};

    let fixture = Fixture::new().await;
    let child = fixture.thread("child").await;
    let target = fixture.thread("parent").await;
    fixture
        .data
        .set_work_state(&child, WorkState::Working)
        .await
        .unwrap();

    fixture
        .data
        .finalize_child_terminal(
            &child,
            AgentStatus::Done,
            WorkState::Idle,
            &target,
            terminal_input(MessageId::new(), "child done"),
        )
        .await
        .unwrap();
    assert_eq!(
        fixture.data.load_meta(&target).await.unwrap().work_state,
        WorkState::Working,
        "EnsureProcessing 结果必须在同一事务内提升接收方工作意图"
    );

    fixture
        .data
        .set_work_state(&target, WorkState::Idle)
        .await
        .unwrap();
    let mut passive = terminal_input(MessageId::new(), "passive result");
    passive.policy = MessagePolicy::passive();
    fixture
        .data
        .finalize_child_terminal(&child, AgentStatus::Done, WorkState::Idle, &target, passive)
        .await
        .unwrap();
    assert_eq!(
        fixture.data.load_meta(&target).await.unwrap().work_state,
        WorkState::Idle,
        "passive 结果不得授予执行资格"
    );
}

/// [回归] 结果接纳失败必须让整次结算回滚：终态已写、MQ 没收到结果正是原缺陷。
///
/// 用「同 ID 已属于别的 thread 历史」注入冲突——冲突只在最后一步出现，前两步若
/// 各自提交就会留在库里。
#[tokio::test]
async fn finalize_child_terminal_rolls_back_when_the_result_cannot_be_enqueued() {
    use peri_acp_types::thread::{AgentStatus, WorkState};

    let fixture = Fixture::new().await;
    let child = fixture.thread("child").await;
    let target = fixture.thread("parent").await;
    let foreign = fixture.thread("foreign").await;
    fixture
        .data
        .set_work_state(&child, WorkState::Working)
        .await
        .unwrap();
    let id = MessageId::new();
    fixture
        .data
        .append_history(
            &foreign,
            &[PersistedPayload::Message(human(id, "already elsewhere"))],
        )
        .await
        .unwrap();

    let error = fixture
        .data
        .finalize_child_terminal(
            &child,
            AgentStatus::Done,
            WorkState::Idle,
            &target,
            terminal_input(id, "child done"),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error.kind(), SessionResourceErrorKind::Conflict { .. }),
        "{error:?}"
    );
    assert_eq!(
        terminal_facts(&fixture, &child).await,
        ("active".to_owned(), Some("working".to_owned())),
        "终态与工作意图都不得越过失败的提交流传"
    );
    assert_eq!(mq_count(&fixture, &target).await, 0);
    assert_eq!(history_len(&fixture, &foreign).await, 1);
}

/// [回归] 同一 child 的终态是最终事实：panic 兜底以同一 task_id 但不同状态/正文
/// 重放结算时不得改写已写下的终态，也不得因 MQ 同 ID 不同正文而永久冲突。
#[tokio::test]
async fn terminal_settlement_is_final_for_a_child() {
    use peri_acp_types::thread::{AgentStatus, WorkState};

    let fixture = Fixture::new().await;
    let child = fixture.thread("child").await;
    let target = fixture.thread("parent").await;
    let id = MessageId::new();
    fixture
        .data
        .finalize_child_terminal(
            &child,
            AgentStatus::Done,
            WorkState::Idle,
            &target,
            terminal_input(id, "result A"),
        )
        .await
        .unwrap();

    // 旧实现：同 ID 不同正文在 `mq_enqueue_fact` 判 Conflict，整次结算回滚后无限重试。
    fixture
        .data
        .finalize_child_terminal(
            &child,
            AgentStatus::Error,
            WorkState::Idle,
            &target,
            terminal_input(id, "panic B"),
        )
        .await
        .expect("已写下的终态必须让重复结算成为成功 no-op");

    assert_eq!(
        terminal_facts(&fixture, &child).await,
        ("done".to_owned(), Some("idle".to_owned())),
        "第二次结算不得改写终态或工作意图"
    );
    let rows = fixture
        .data
        .mq_query(&target, MqFilter::default())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "不得新增结果行");
    match &rows[0].payload {
        PersistedPayload::SystemReminder { reminder, .. } => {
            assert_eq!(reminder.as_reminder().body, "result A");
        }
        other => panic!("unexpected payload {other:?}"),
    }
}

/// [回归] 同状态、不同正文的重复结算（panic 兜底）也必须被吞掉：判据是结果 ID
/// 已落地，而不是「状态是否相同」。
#[tokio::test]
async fn terminal_settlement_ignores_a_repeat_with_the_same_status() {
    use peri_acp_types::thread::{AgentStatus, WorkState};

    let fixture = Fixture::new().await;
    let child = fixture.thread("child").await;
    let target = fixture.thread("parent").await;
    let id = MessageId::new();
    fixture
        .data
        .finalize_child_terminal(
            &child,
            AgentStatus::Error,
            WorkState::Idle,
            &target,
            terminal_input(id, "result A"),
        )
        .await
        .unwrap();

    // 旧判据只看状态：同状态不早退，继续入队时同 ID 不同正文判 Conflict。
    fixture
        .data
        .finalize_child_terminal(
            &child,
            AgentStatus::Error,
            WorkState::Idle,
            &target,
            terminal_input(id, "panic B"),
        )
        .await
        .expect("结果 ID 已落地时重复结算必须成功 no-op");

    assert_eq!(
        terminal_facts(&fixture, &child).await,
        ("error".to_owned(), Some("idle".to_owned())),
        "重复结算不得改写终态或工作意图"
    );
    let rows = fixture
        .data
        .mq_query(&target, MqFilter::default())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "不得新增结果行");
    match &rows[0].payload {
        PersistedPayload::SystemReminder { reminder, .. } => {
            assert_eq!(reminder.as_reminder().body, "result A");
        }
        other => panic!("unexpected payload {other:?}"),
    }
}

/// 同一状态重放仍走原有幂等路径：两次都成功且只有一行。
#[tokio::test]
async fn terminal_settlement_stays_idempotent_for_the_same_status() {
    use peri_acp_types::thread::{AgentStatus, WorkState};

    let fixture = Fixture::new().await;
    let child = fixture.thread("child").await;
    let target = fixture.thread("parent").await;
    let id = MessageId::new();
    for _ in 0..2 {
        fixture
            .data
            .finalize_child_terminal(
                &child,
                AgentStatus::Done,
                WorkState::Idle,
                &target,
                terminal_input(id, "child done"),
            )
            .await
            .unwrap();
    }
    assert_eq!(mq_count(&fixture, &target).await, 1);
    assert_eq!(
        terminal_facts(&fixture, &child).await,
        ("done".to_owned(), Some("idle".to_owned()))
    );
}

/// [回归] 关闭结算的第一步写成功、第二步失败时必须整批回滚。
///
/// 两条提醒里第二条身份冲突：旧实现「先写提醒、再删 MQ」会留下已写提醒 + 未删行。
#[tokio::test]
async fn settle_retained_mq_rolls_back_history_and_rows_together() {
    use peri_acp_types::session::RetainedReminder;

    let fixture = Fixture::new().await;
    let thread = fixture.thread("child").await;
    let entry = fixture
        .data
        .mq_enqueue(&thread, input(MessageId::new(), "retained"))
        .await
        .unwrap();
    let accepted = MessageId::new();
    fixture
        .data
        .append_history(
            &thread,
            &[PersistedPayload::Message(human(accepted, "already there"))],
        )
        .await
        .unwrap();
    let before = history_len(&fixture, &thread).await;

    let fresh = MessageId::new();
    let error = fixture
        .data
        .settle_retained_mq(
            &thread,
            &[
                RetainedReminder {
                    id: fresh,
                    reminder: reminder("new settlement"),
                },
                RetainedReminder {
                    id: accepted,
                    reminder: reminder("conflicting settlement"),
                },
            ],
            std::slice::from_ref(&entry.entry),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error.kind(), SessionResourceErrorKind::InvalidInput { .. }),
        "{error:?}"
    );
    assert_eq!(
        history_len(&fixture, &thread).await,
        before,
        "第一条提醒也不得写入"
    );
    assert_eq!(mq_count(&fixture, &thread).await, 1, "待发行原样保留");

    // 同样的输入去掉冲突项后整体生效：提醒写入且 MQ 行删除。
    fixture
        .data
        .settle_retained_mq(
            &thread,
            &[RetainedReminder {
                id: fresh,
                reminder: reminder("new settlement"),
            }],
            std::slice::from_ref(&entry.entry),
        )
        .await
        .unwrap();
    assert_eq!(history_len(&fixture, &thread).await, before + 1);
    assert_eq!(mq_count(&fixture, &thread).await, 0);
}

// ─── 迁移：v13 → v14、v12 → v14 与 v11 → v14 ─────────────────────────────────

/// 把库降级成 v13 形状（删两列工作快照）或更早形状（再删 `thread_mq`）后重开：
/// 幂等补齐并推进到当前版本。
#[tokio::test]
async fn migration_adds_thread_mq_from_the_previous_generation() {
    let fixture = Fixture::new().await;
    let thread = fixture.thread("mq").await;
    let id = MessageId::new();
    fixture
        .data
        .mq_enqueue(&thread, input(id, "before downgrade"))
        .await
        .unwrap();
    let path = fixture.path();
    fixture.store.close().await;
    downgrade(&path, 12).await;

    let reopened = SqliteThreadStore::new(&path).await.unwrap();
    assert_mq_shape(&reopened).await;
    let (version,): (i64,) = sqlx::query_as("PRAGMA user_version")
        .fetch_one(&reopened.database.pool)
        .await
        .unwrap();
    assert_eq!(version, canonical::CURRENT_SCHEMA_VERSION);
    reopened.close().await;
}

/// v11 形状（缺 inherited 派生索引表与 `thread_mq`）走同一条幂等补齐路径。
#[tokio::test]
async fn migration_adds_thread_mq_from_the_legacy_current_generation() {
    let fixture = Fixture::new().await;
    let thread = fixture.thread("mq").await;
    fixture
        .data
        .mq_enqueue(&thread, input(MessageId::new(), "kept?"))
        .await
        .unwrap();
    let path = fixture.path();
    fixture.store.close().await;
    downgrade(&path, 11).await;

    let reopened = SqliteThreadStore::new(&path).await.unwrap();
    assert_mq_shape(&reopened).await;
    let (version,): (i64,) = sqlx::query_as("PRAGMA user_version")
        .fetch_one(&reopened.database.pool)
        .await
        .unwrap();
    assert_eq!(version, canonical::CURRENT_SCHEMA_VERSION);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
        .fetch_one(&reopened.database.pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "会话事实不因迁移被改写");
    reopened.close().await;
}

async fn downgrade(path: &Path, version: i64) {
    // 形状退回与 `schema_v14_test` 共用同一份夹具：v15 改形的三张表必须整表重建，
    // 否则列序（`identity` 在 `discovery` 之前）不满足旧代际的形状判定。
    super::schema_v14_tests::downgrade(path, version, true).await;
}

async fn assert_mq_shape(store: &SqliteThreadStore) {
    let (table,): (String,) = sqlx::query_as(
        "SELECT name FROM sqlite_schema WHERE type = 'table' AND name = 'thread_mq'",
    )
    .fetch_one(&store.database.pool)
    .await
    .unwrap();
    assert_eq!(table, "thread_mq");
    let (index,): (String,) = sqlx::query_as(
        "SELECT name FROM sqlite_schema WHERE type = 'index' AND name = 'idx_thread_mq_order'",
    )
    .fetch_one(&store.database.pool)
    .await
    .unwrap();
    assert_eq!(index, "idx_thread_mq_order");
}
