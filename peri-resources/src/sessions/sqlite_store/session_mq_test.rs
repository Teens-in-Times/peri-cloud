//! Thread MQ 的本机行为：真实 SQLite 文件上的入队、投影、取消、原子移交、清理与迁移。
//!
//! 断言以**可观察事实**为准：重开数据库后的行、历史表与 MQ 表的实际内容、以及两种竞争结果
//! 的互斥。夹具用真实临时文件而不是 `:memory:`——「提交前后重开」只有落盘才可验证。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use peri_acp_types::messages::{BaseMessage, MessageId};
use peri_acp_types::session::{
    MessageKind, MessagePolicy, MessageSource, MqCancelOutcome, MqEnqueueDisposition, MqFilter,
    MqInput, MqReceiveDisposition, MqReceiveItem, MqReceivePurpose, MqReceiveSelection,
    MqReceiveStatus,
};
use peri_acp_types::session_resources::{
    RewindBoundary, SessionResourceErrorKind, TerminalSettlementOutcome,
};
use peri_acp_types::store::PersistedPayload;
use peri_acp_types::thread::{ThreadId, ThreadMeta};

use super::*;
use crate::sessions::canonical;
use crate::sessions::data::SessionDataPort;

#[path = "session_mq_terminal_test.rs"]
mod terminal_settlement;

struct Fixture {
    directory: tempfile::TempDir,
    store: SqliteThreadStore,
    data: SqliteSessionData,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = SqliteThreadStore::new(directory.path().join("threads.db"))
            .await
            .unwrap();
        let data = SqliteSessionData::new(Arc::clone(&store.database));
        Self {
            directory,
            store,
            data,
        }
    }

    fn path(&self) -> PathBuf {
        self.directory.path().join("threads.db")
    }

    async fn thread(&self, name: &str) -> ThreadId {
        self.store
            .create_thread(ThreadMeta::new_at(
                self.directory.path().to_string_lossy().into_owned(),
                peri_time::now_wall(),
            ))
            .await
            .unwrap_or_else(|error| panic!("{name}: {error}"))
    }
}

/// 一条用户输入：payload 携带**同一个**稳定 ID（MQ 与最终历史共用身份）。
fn input(id: MessageId, text: &str) -> MqInput {
    MqInput {
        id,
        kind: MessageKind::Prompt,
        source: MessageSource::UserInput,
        payload: PersistedPayload::Message(human(id, text)),
        policy: MessagePolicy::ensure_processing(),
        original_draft: Some(text.to_owned()),
    }
}

fn human(id: MessageId, text: &str) -> BaseMessage {
    BaseMessage::Human {
        id,
        content: text.into(),
    }
}

fn selection(
    entry: &peri_acp_types::session::MqEntry,
    disposition: MqReceiveDisposition,
) -> MqReceiveSelection {
    MqReceiveSelection {
        items: vec![MqReceiveItem {
            entry: entry.clone(),
            disposition,
        }],
        purpose: MqReceivePurpose::ModelLoop,
        execution: peri_acp_types::session::ExecutionBinding {
            turn_id: peri_acp_types::session::TurnId::new(),
            attempt_id: peri_acp_types::identity::AttemptId::new(),
        },
    }
}

async fn count(pool: &sqlx::SqlitePool, sql: &str, thread: &ThreadId) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
        .bind(thread.as_str())
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn mq_count(fixture: &Fixture, thread: &ThreadId) -> i64 {
    count(
        &fixture.store.database.pool,
        "SELECT COUNT(*) FROM thread_mq WHERE thread_id = ?1",
        thread,
    )
    .await
}

async fn history_len(fixture: &Fixture, thread: &ThreadId) -> i64 {
    count(
        &fixture.store.database.pool,
        "SELECT COUNT(*) FROM messages WHERE thread_id = ?1",
        thread,
    )
    .await
}

/// 入队后的完整事实在重开后逐项一致（内容、时间、原稿、策略、目标 thread）。
#[tokio::test]
async fn enqueue_survives_reopen_with_every_fact() {
    let fixture = Fixture::new().await;
    let thread = fixture.thread("mq").await;
    let id = MessageId::new();
    let requested = MqInput {
        id,
        kind: MessageKind::Defer,
        source: MessageSource::SubAgentComplete,
        payload: PersistedPayload::Message(human(id, "child done")),
        policy: MessagePolicy::passive(),
        original_draft: Some("draft text".to_owned()),
    };
    let outcome = fixture
        .data
        .mq_enqueue(&thread, requested.clone())
        .await
        .unwrap();
    assert_eq!(outcome.disposition, MqEnqueueDisposition::Inserted);
    assert_eq!(outcome.entry.id, id);
    assert_eq!(outcome.entry.original_draft.as_deref(), Some("draft text"));
    assert_micros_utc(&outcome.entry.enqueued_at);

    let path = fixture.path();
    fixture.store.close().await;
    let reopened = SqliteThreadStore::new(&path).await.unwrap();
    let data = SqliteSessionData::new(Arc::clone(&reopened.database));
    let entries = data.mq_query(&thread, MqFilter::default()).await.unwrap();
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(
        entry.enqueued_at, outcome.entry.enqueued_at,
        "重开不得改写入队时间"
    );
    assert_eq!(entry.kind, MessageKind::Defer);
    assert_eq!(entry.source, MessageSource::SubAgentComplete);
    assert_eq!(entry.policy, MessagePolicy::passive());
    assert_eq!(entry.original_draft.as_deref(), Some("draft text"));
    assert_eq!(
        peri_acp_types::store::serialize_persisted_payload(&entry.payload).unwrap(),
        peri_acp_types::store::serialize_persisted_payload(&requested.payload).unwrap()
    );
    reopened.close().await;
}

fn assert_micros_utc(value: &str) {
    // `YYYY-MM-DDTHH:MM:SS.ffffffZ`：固定六位小数 + Z。
    assert_eq!(value.len(), 27, "{value}");
    assert!(value.ends_with('Z'), "{value}");
    let fraction = &value[20..26];
    assert!(
        fraction.chars().all(|c| c.is_ascii_digit()),
        "六位小数: {value}"
    );
    assert_eq!(&value[19..20], ".", "{value}");
}

/// 查询按 `(enqueued_at, id)` 升序，四种过滤各按自己的口径收敛。
#[tokio::test]
async fn query_orders_and_applies_each_filter() {
    let fixture = Fixture::new().await;
    let thread = fixture.thread("mq").await;
    let first = MessageId::new();
    let second = MessageId::new();
    let third = MessageId::new();
    fixture
        .data
        .mq_enqueue(&thread, input(first, "first"))
        .await
        .unwrap();
    fixture
        .data
        .mq_enqueue(
            &thread,
            MqInput {
                kind: MessageKind::Defer,
                source: MessageSource::ShellComplete,
                ..input(second, "second")
            },
        )
        .await
        .unwrap();
    fixture
        .data
        .mq_enqueue(
            &thread,
            MqInput {
                kind: MessageKind::Info,
                source: MessageSource::SystemInjected,
                original_draft: None,
                ..input(third, "third")
            },
        )
        .await
        .unwrap();

    let all = fixture
        .data
        .mq_query(&thread, MqFilter::default())
        .await
        .unwrap();
    assert_eq!(
        all.iter().map(|entry| entry.id).collect::<Vec<_>>(),
        vec![first, second, third],
        "入队顺序即 (enqueued_at, id) 升序"
    );

    let user_only = fixture
        .data
        .mq_query(
            &thread,
            MqFilter {
                user_input_only: true,
                ..MqFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(user_only.len(), 1);
    assert_eq!(user_only[0].id, first);

    let shells = fixture
        .data
        .mq_query(
            &thread,
            MqFilter {
                sources: vec![MessageSource::ShellComplete, MessageSource::SystemInjected],
                ..MqFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        shells.iter().map(|entry| entry.id).collect::<Vec<_>>(),
        vec![second, third]
    );

    let after = fixture
        .data
        .mq_query(
            &thread,
            MqFilter {
                after: Some(all[0].watermark()),
                ..MqFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        after.iter().map(|entry| entry.id).collect::<Vec<_>>(),
        vec![second, third],
        "水位是严格晚于"
    );

    let limited = fixture
        .data
        .mq_query(
            &thread,
            MqFilter {
                limit: Some(2),
                ..MqFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        limited.iter().map(|entry| entry.id).collect::<Vec<_>>(),
        vec![first, second],
        "上限按升序取前 N 条"
    );
    assert_eq!(mq_count(&fixture, &thread).await, 3, "查询不消费");

    // 控制面投影与完整行共享同一份过滤语义。
    let header_ids = |headers: Vec<peri_acp_types::session::MqHeader>| {
        headers
            .into_iter()
            .map(|header| header.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        header_ids(
            fixture
                .data
                .mq_headers(&thread, MqFilter::default())
                .await
                .unwrap()
        ),
        vec![first, second, third]
    );
    assert_eq!(
        header_ids(
            fixture
                .data
                .mq_headers(
                    &thread,
                    MqFilter {
                        sources: vec![MessageSource::ShellComplete, MessageSource::SystemInjected],
                        limit: Some(1),
                        ..MqFilter::default()
                    }
                )
                .await
                .unwrap()
        ),
        vec![second],
        "来源过滤与上限同样在 header 查询生效"
    );
    assert_eq!(
        header_ids(
            fixture
                .data
                .mq_headers(
                    &thread,
                    MqFilter {
                        after: Some(all[0].watermark()),
                        ..MqFilter::default()
                    }
                )
                .await
                .unwrap()
        ),
        vec![second, third],
        "水位是严格晚于"
    );
    assert_eq!(
        fixture.data.mq_watermark(&thread).await.unwrap(),
        Some(all[2].watermark())
    );
    assert_eq!(fixture.data.mq_count(&thread).await.unwrap(), 3);
    assert!(fixture.data.mq_exists(&thread).await.unwrap());
}

/// 128 条 64 KiB 正文：控制面查询与有界正文查询不得解码全部正文。
///
/// 把最后一条正文写成不可解析值：控制面（header/count/exists/水位）与
/// `LIMIT 1` 仍须成功；只有无界正文查询失败关闭。这直接证明查询形状，
/// 而不是靠耗时或内存猜测。
#[tokio::test]
async fn control_plane_queries_do_not_decode_mq_payloads() {
    let fixture = Fixture::new().await;
    let thread = fixture.thread("mq").await;
    let body = "x".repeat(64 * 1024);
    for _ in 0..128 {
        let id = MessageId::new();
        fixture
            .data
            .mq_enqueue(
                &thread,
                MqInput {
                    id,
                    kind: MessageKind::Defer,
                    source: MessageSource::SubAgentComplete,
                    payload: PersistedPayload::Message(human(id, &body)),
                    policy: MessagePolicy::ensure_processing(),
                    original_draft: None,
                },
            )
            .await
            .unwrap();
    }
    let headers = fixture
        .data
        .mq_headers(&thread, MqFilter::default())
        .await
        .unwrap();
    assert_eq!(headers.len(), 128);
    let max_watermark = headers.last().unwrap().watermark();
    assert_eq!(
        fixture.data.mq_watermark(&thread).await.unwrap(),
        Some(max_watermark)
    );
    assert_eq!(fixture.data.mq_count(&thread).await.unwrap(), 128);
    assert!(fixture.data.mq_exists(&thread).await.unwrap());

    sqlx::query("UPDATE thread_mq SET payload_json = 'not-json' WHERE thread_id = ?1 AND id = ?2")
        .bind(thread.as_str())
        .bind(headers.last().unwrap().id.as_uuid().to_string())
        .execute(&fixture.store.database.pool)
        .await
        .unwrap();

    assert!(fixture.data.mq_exists(&thread).await.unwrap());
    assert_eq!(fixture.data.mq_count(&thread).await.unwrap(), 128);
    assert_eq!(
        fixture
            .data
            .mq_headers(&thread, MqFilter::default())
            .await
            .unwrap()
            .len(),
        128,
        "header 查询不得因正文损坏失败"
    );
    assert_eq!(
        fixture
            .data
            .mq_query(
                &thread,
                MqFilter {
                    limit: Some(1),
                    ..MqFilter::default()
                }
            )
            .await
            .unwrap()
            .len(),
        1,
        "LIMIT 必须在 SQL 侧生效，不能全量取回再过滤"
    );
    assert!(
        fixture
            .data
            .mq_query(&thread, MqFilter::default())
            .await
            .is_err(),
        "无界正文查询必须如实报告损坏，不能静默跳过"
    );
}

/// 同 ID 同内容复用原接纳结果；不同内容或跨 thread 是身份冲突。
#[tokio::test]
async fn same_id_is_idempotent_but_a_different_message_conflicts() {
    let fixture = Fixture::new().await;
    let thread = fixture.thread("mq").await;
    let other = fixture.thread("other").await;
    let id = MessageId::new();
    let first = fixture
        .data
        .mq_enqueue(&thread, input(id, "same"))
        .await
        .unwrap();
    let again = fixture
        .data
        .mq_enqueue(&thread, input(id, "same"))
        .await
        .unwrap();
    assert_eq!(again.disposition, MqEnqueueDisposition::Existing);
    assert_eq!(again.entry.enqueued_at, first.entry.enqueued_at);
    assert_eq!(mq_count(&fixture, &thread).await, 1);

    let conflict = fixture
        .data
        .mq_enqueue(&thread, input(id, "different"))
        .await
        .unwrap_err();
    assert!(
        matches!(conflict.kind(), SessionResourceErrorKind::Conflict { .. }),
        "{conflict:?}"
    );

    let cross = fixture
        .data
        .mq_enqueue(&other, input(id, "same"))
        .await
        .unwrap_err();
    assert!(
        matches!(cross.kind(), SessionResourceErrorKind::Conflict { .. }),
        "同一 ID 属于别的 thread: {cross:?}"
    );
    assert_eq!(mq_count(&fixture, &other).await, 0);
}

/// 已移交的 ID 再入队只报告 AlreadyHandedOff，不产生第二条待发副本。
#[tokio::test]
async fn enqueue_after_receive_reports_already_handed_off() {
    let fixture = Fixture::new().await;
    let thread = fixture.thread("mq").await;
    let other = fixture.thread("other").await;
    let id = MessageId::new();
    let entry = fixture
        .data
        .mq_enqueue(&thread, input(id, "hand off"))
        .await
        .unwrap();
    let received = fixture
        .data
        .mq_receive(
            &thread,
            selection(&entry.entry, MqReceiveDisposition::Commit),
        )
        .await
        .unwrap();
    assert_eq!(received.items[0].status, MqReceiveStatus::Committed);
    assert_eq!(history_len(&fixture, &thread).await, 1);
    assert_eq!(mq_count(&fixture, &thread).await, 0);

    let again = fixture
        .data
        .mq_enqueue(&thread, input(id, "hand off"))
        .await
        .unwrap();
    assert_eq!(again.disposition, MqEnqueueDisposition::AlreadyHandedOff);
    assert_eq!(mq_count(&fixture, &thread).await, 0);

    let cross = fixture
        .data
        .mq_enqueue(&other, input(id, "hand off"))
        .await
        .unwrap_err();
    assert!(matches!(
        cross.kind(),
        SessionResourceErrorKind::Conflict { .. }
    ));
}

/// 取消只接受用户输入；移交过的 ID 报 AlreadyHandedOff，空 ID 报 NotFound。
#[tokio::test]
async fn cancel_returns_the_row_and_refuses_non_user_input() {
    let fixture = Fixture::new().await;
    let thread = fixture.thread("mq").await;
    let id = MessageId::new();
    fixture
        .data
        .mq_enqueue(&thread, input(id, "cancel me"))
        .await
        .unwrap();
    let outcome = fixture.data.mq_cancel(&thread, id).await.unwrap();
    let MqCancelOutcome::Cancelled(entry) = outcome else {
        panic!("expected a cancelled row");
    };
    assert_eq!(entry.original_draft.as_deref(), Some("cancel me"));
    assert_eq!(mq_count(&fixture, &thread).await, 0);
    assert_eq!(history_len(&fixture, &thread).await, 0, "取消不写历史");
    assert!(matches!(
        fixture.data.mq_cancel(&thread, id).await.unwrap(),
        MqCancelOutcome::NotFound
    ));

    let deferred = MessageId::new();
    fixture
        .data
        .mq_enqueue(
            &thread,
            MqInput {
                kind: MessageKind::Defer,
                source: MessageSource::SubAgentComplete,
                ..input(deferred, "cannot cancel")
            },
        )
        .await
        .unwrap();
    let error = fixture.data.mq_cancel(&thread, deferred).await.unwrap_err();
    assert!(matches!(
        error.kind(),
        SessionResourceErrorKind::InvalidInput { .. }
    ));
    assert_eq!(mq_count(&fixture, &thread).await, 1, "拒绝不得删行");
}

include!("session_mq_receive_test.rs");
