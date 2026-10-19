use super::*;
use crate::state::CloudJournal;
use sqlx::Row;

pub(super) async fn fixture() -> (tempfile::TempDir, CloudJournal, Uuid, Uuid) {
    let root = tempfile::tempdir().unwrap();
    let journal = CloudJournal::open(root.path()).await.unwrap();
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    for id in [a, b] {
        sqlx::query("INSERT INTO principals(id,login,display_name,password_hash) VALUES(?,?,?,'fixture-not-a-password-hash')")
            .bind(id.to_string()).bind(id.to_string()).bind("Fixture").execute(&journal.identity_pool()).await.unwrap();
    }
    (root, journal, a, b)
}
pub(super) fn value(content: &str) -> MemoryContent {
    MemoryContent {
        content: content.into(),
        kind: MemoryKind::Fact,
        pinned: false,
        valid_until: None,
    }
}

#[tokio::test]
async fn schema_eleven_imports_original_once_and_removes_the_old_contract() {
    let (root, journal, a, b) = fixture().await;
    let pool = journal.identity_pool();
    for sql in [
        "DROP TABLE cloud_native_cancellations", "DROP TABLE cloud_native_operations", "DROP TABLE cloud_device_targets", "DROP TABLE agent_task_instruction_reads", "DROP TABLE agent_task_approvals", "DROP TABLE agent_task_runs", "DROP TABLE agent_task_operations", "DROP TABLE agent_task_checkpoints",
        "DROP TABLE agent_execution_windows", "DROP TABLE agent_execution_sessions",
        "DROP TABLE agent_task_trace", "DROP TABLE agent_task_messages",
        "DROP TABLE agent_device_leases", "DROP TABLE agent_tasks",
        "DROP TABLE gateway_queue", "DROP TABLE conversation_projections",
        "DROP TABLE memory_session_audience", "DROP TABLE memory_fts","DROP TABLE memory_audit","DROP TABLE memory_items","DROP TABLE memory_accounts","DROP TABLE user_preferences",
        "CREATE TABLE user_data (principal TEXT PRIMARY KEY REFERENCES principals(id),revision INTEGER NOT NULL,instructions TEXT NOT NULL,memory TEXT NOT NULL)",
        "PRAGMA user_version=11",
    ] {sqlx::query(sql).execute(&pool).await.unwrap();}
    let original = "旧版的一整段资料\n东京是测试环境，不拆分伪造来源。";
    sqlx::query("INSERT INTO user_data VALUES(?,4,'原偏好',?)")
        .bind(a.to_string())
        .bind(original)
        .execute(&pool)
        .await
        .unwrap();
    journal.close().await;
    drop(pool);
    drop(journal);
    let journal = CloudJournal::open(root.path()).await.unwrap();
    let store = MemoryStore::new(&journal, a, 65536);
    let page = store.list("", 0, 5).await.unwrap();
    assert_eq!(page.items.len(), 1);
    let imported = &page.items[0];
    assert_eq!(imported.value.content, original);
    assert_eq!(imported.source.kind, "legacy_import");
    assert!(imported.source.turn_id.is_none());
    assert_eq!(imported.created_at, 0);
    assert_eq!(journal.preferences(a).await.unwrap().revision, 4);
    assert_eq!(journal.preferences(a).await.unwrap().instructions, "原偏好");
    assert!(MemoryStore::new(&journal, b, 65536)
        .list("", 0, 5)
        .await
        .unwrap()
        .items
        .is_empty());
    let (count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM sqlite_master WHERE name='user_data'")
            .fetch_one(&journal.identity_pool())
            .await
            .unwrap();
    assert_eq!(count, 0);
    let id = imported.id;
    journal.close().await;
    drop(store);
    drop(journal);
    let journal = CloudJournal::open(root.path()).await.unwrap();
    let store = MemoryStore::new(&journal, a, 65536);
    let page = store.list("东京", 0, 5).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, id);
}

#[tokio::test]
async fn chinese_short_words_paths_entities_and_fts_syntax_are_owner_scoped() {
    let (_root, journal, a, b) = fixture().await;
    let store = MemoryStore::new(&journal, a, 65536);
    let private = MemoryStore::new(&journal, b, 65536);
    store
        .add(
            Uuid::new_v4(),
            0,
            value("东京的测试服务器叫小蓝，工作目录 D:\\deploy\\a_b，ID node-17"),
            MemorySource::web(),
        )
        .await
        .unwrap();
    private
        .add(
            Uuid::new_v4(),
            0,
            value("东京的私人地址 B_ONLY"),
            MemorySource::web(),
        )
        .await
        .unwrap();
    for query in [
        "东京",
        "小蓝",
        "蓝",
        "D:\\deploy\\a_b",
        "node-17",
        "测试服务器",
    ] {
        let found = store.list(query, 0, 5).await.unwrap();
        assert_eq!(found.items.len(), 1, "{query}");
        assert!(!serde_json::to_string(&found).unwrap().contains("B_ONLY"));
    }
    assert!(store
        .list("\" OR * ) - :", 0, 5)
        .await
        .unwrap()
        .items
        .is_empty());
    let foreign = private.list("", 0, 5).await.unwrap().items.remove(0).id;
    assert!(matches!(
        store.get(foreign).await,
        Err(MemoryError::NotFound)
    ));
    assert!(matches!(
        store.forget(foreign, 1, MemorySource::web()).await,
        Err(MemoryError::Conflict)
    ));
}

#[tokio::test]
async fn concurrent_corrections_are_cas_and_index_changes_atomically() {
    let (_root, journal, a, _) = fixture().await;
    let store = MemoryStore::new(&journal, a, 65536);
    let original = store
        .add(Uuid::new_v4(), 0, value("原来的事实"), MemorySource::web())
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        store.update(original.id, 1, value("新事实甲"), MemorySource::web()),
        store.update(original.id, 1, value("新事实乙"), MemorySource::web())
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let latest = store.get(original.id).await.unwrap();
    assert_eq!(latest.revision, 2);
    assert_eq!(store.list("", 0, 5).await.unwrap().generation, 2);
    assert!(store.list("原来的", 0, 5).await.unwrap().items.is_empty());
    assert_eq!(
        store
            .list(&latest.value.content, 0, 5)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
}

#[tokio::test]
async fn forgetting_erases_body_and_index_and_rejects_stale_add_after_reopen() {
    let (root, journal, a, _) = fixture().await;
    let store = MemoryStore::new(&journal, a, 65536);
    let id = Uuid::new_v4();
    let content = value("FORGOTTEN_UNIQUE_BODY");
    store
        .add(id, 0, content.clone(), MemorySource::web())
        .await
        .unwrap();
    store.forget(id, 1, MemorySource::web()).await.unwrap();
    store.rebuild_index().await.unwrap();
    assert!(store
        .list("FORGOTTEN_UNIQUE_BODY", 0, 5)
        .await
        .unwrap()
        .items
        .is_empty());
    assert!(matches!(store.get(id).await, Err(MemoryError::NotFound)));
    assert!(matches!(
        store.add(id, 0, content.clone(), MemorySource::web()).await,
        Err(MemoryError::Conflict)
    ));
    assert!(matches!(
        store
            .add(Uuid::new_v4(), 1, content.clone(), MemorySource::web())
            .await,
        Err(MemoryError::Conflict)
    ));
    let row = sqlx::query("SELECT content,search_text FROM memory_items WHERE id=?")
        .bind(id.to_string())
        .fetch_one(&journal.identity_pool())
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("content"), "");
    assert_eq!(row.get::<String, _>("search_text"), "");
    journal.close().await;
    drop(store);
    drop(journal);
    let reopened = CloudJournal::open(root.path()).await.unwrap();
    let store = MemoryStore::new(&reopened, a, 65536);
    store.rebuild_index().await.unwrap();
    assert!(store.list("", 0, 5).await.unwrap().items.is_empty());
    // An explicit new request made after observing deletion may relearn a fact.
    store
        .add(Uuid::new_v4(), 2, content, MemorySource::web())
        .await
        .unwrap();
}

#[tokio::test]
async fn expired_items_are_not_recalled_but_can_be_managed_and_renewed() {
    let (_root, journal, a, _) = fixture().await;
    let store = MemoryStore::new(&journal, a, 65536);
    let mut content = value("临时观测");
    content.pinned = true;
    content.valid_until = Some(now() + 30);
    let item = store
        .add(Uuid::new_v4(), 0, content, MemorySource::web())
        .await
        .unwrap();
    sqlx::query("UPDATE memory_items SET valid_until=1 WHERE id=?")
        .bind(item.id.to_string())
        .execute(&journal.identity_pool())
        .await
        .unwrap();
    assert!(store.list("", 0, 5).await.unwrap().items.is_empty());
    assert!(store.snapshot().await.unwrap().items.is_empty());
    assert!(matches!(
        store.get(item.id).await,
        Err(MemoryError::NotFound)
    ));
    assert_eq!(store.inventory("", 0, 5).await.unwrap().items.len(), 1);
    let expired = store.inspect(item.id).await.unwrap();
    store
        .update(
            item.id,
            expired.revision,
            value("已重新确认"),
            MemorySource::web(),
        )
        .await
        .unwrap();
    assert_eq!(
        store.get(item.id).await.unwrap().value.content,
        "已重新确认"
    );
}

#[tokio::test]
async fn quotas_use_utf8_bytes_and_failed_write_changes_neither_generation_nor_index() {
    let (_root, journal, a, _) = fixture().await;
    let store = MemoryStore::new(&journal, a, 10);
    let item = store
        .add(Uuid::new_v4(), 0, value("中文"), MemorySource::web())
        .await
        .unwrap();
    assert!(matches!(
        store
            .update(item.id, 1, value("中文超过十字节"), MemorySource::web())
            .await,
        Err(MemoryError::Limit)
    ));
    let page = store.list("", 0, 5).await.unwrap();
    assert_eq!((page.generation, page.used_bytes, page.total), (1, 6, 1));
    assert_eq!(store.get(item.id).await.unwrap().revision, 1);
    assert!(store.list("超过", 0, 5).await.unwrap().items.is_empty());
}

#[tokio::test]
async fn pinned_snapshot_and_search_respect_serialized_byte_budgets() {
    let (_root, journal, a, _) = fixture().await;
    let store = MemoryStore::new(&journal, a, 65536);
    for generation in 0..8 {
        let mut content = value(&format!("{generation}{}", "记忆\n\"".repeat(200)));
        content.pinned = true;
        store
            .add(Uuid::new_v4(), generation, content, MemorySource::web())
            .await
            .unwrap();
    }
    let snap = store.snapshot().await.unwrap();
    assert!(snap.omitted > 0);
    assert!(
        snap.items
            .iter()
            .map(|i| i.value.content.chars().count())
            .sum::<usize>()
            <= SNAPSHOT_CHARS
    );
    assert!(
        snap.items
            .iter()
            .map(|i| serde_json::to_vec(i).unwrap().len())
            .sum::<usize>()
            <= SNAPSHOT_BYTES
    );
    let mut offset = 0;
    let mut seen = std::collections::HashSet::new();
    loop {
        let page = store.list("", offset, 20).await.unwrap();
        assert!(
            page.items
                .iter()
                .map(|i| serde_json::to_vec(i).unwrap().len())
                .sum::<usize>()
                <= RESULT_BYTES
        );
        for i in page.items {
            assert!(seen.insert(i.id));
        }
        if let Some(next) = page.next_offset {
            assert!(next > offset);
            offset = next;
        } else {
            break;
        }
    }
    assert_eq!(seen.len(), 8);
}
