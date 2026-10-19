use super::store::McpServerRecord;
use super::*;
use crate::state::CloudJournal;
use uuid::Uuid;

pub(super) async fn fixture() -> (tempfile::TempDir, CloudJournal, Uuid, Uuid) {
    let root = tempfile::tempdir().unwrap();
    let journal = CloudJournal::open(root.path()).await.unwrap();
    let (owner, other) = (Uuid::new_v4(), Uuid::new_v4());
    for id in [owner, other] {
        sqlx::query(
            "INSERT INTO principals(id,login,display_name,password_hash) VALUES(?,?,?,'fixture-not-a-password-hash')",
        )
        .bind(id.to_string())
        .bind(id.to_string())
        .bind("Fixture")
        .execute(&journal.identity_pool())
        .await
        .unwrap();
    }
    (root, journal, owner, other)
}

fn store(journal: &CloudJournal, principal: Uuid) -> McpServerStore {
    McpServerStore::new(journal.identity_pool(), principal)
}

#[tokio::test]
async fn servers_are_added_replaced_disabled_and_removed_with_revisions() {
    let (_root, journal, owner, _other) = fixture().await;
    let store = store(&journal, owner);
    let (version,): (i64,) = sqlx::query_as("PRAGMA user_version")
        .fetch_one(&journal.identity_pool())
        .await
        .unwrap();
    assert_eq!(version, 21);
    assert_eq!(store.revision().await.unwrap(), 0);
    assert!(store.list().await.unwrap().is_empty());

    store
        .add("docs", "https://93.184.216.34/mcp")
        .await
        .unwrap();
    assert_eq!(store.revision().await.unwrap(), 1);
    let listed = store.list().await.unwrap();
    assert_eq!(
        listed,
        vec![McpServerRecord {
            name: "docs".into(),
            url: "https://93.184.216.34/mcp".into(),
            disabled: false,
        }]
    );

    store
        .add("docs", "https://93.184.216.35/mcp")
        .await
        .unwrap();
    assert_eq!(store.revision().await.unwrap(), 2);
    assert_eq!(store.list().await.unwrap().len(), 1);
    assert_eq!(
        store.enabled().await.unwrap()[0].url,
        "https://93.184.216.35/mcp"
    );

    store.set_disabled("docs", true).await.unwrap();
    assert_eq!(store.revision().await.unwrap(), 3);
    assert!(store.enabled().await.unwrap().is_empty());
    assert!(store.list().await.unwrap()[0].disabled);

    store.remove("docs").await.unwrap();
    assert_eq!(store.revision().await.unwrap(), 4);
    assert!(store.list().await.unwrap().is_empty());
    assert_eq!(
        store.remove("docs").await.unwrap_err(),
        McpConfigError::NotFound
    );
}

#[tokio::test]
async fn accounts_cannot_see_or_edit_each_others_servers() {
    let (_root, journal, owner, other) = fixture().await;
    let owner_store = store(&journal, owner);
    let other_store = store(&journal, other);
    owner_store
        .add("docs", "https://93.184.216.34/mcp")
        .await
        .unwrap();

    assert!(other_store.list().await.unwrap().is_empty());
    assert_eq!(other_store.revision().await.unwrap(), 0);
    assert_eq!(
        other_store.remove("docs").await.unwrap_err(),
        McpConfigError::NotFound
    );
    assert_eq!(owner_store.list().await.unwrap().len(), 1);
}

#[tokio::test]
async fn invalid_names_and_egress_targets_are_rejected_before_storage() {
    let (_root, journal, owner, _other) = fixture().await;
    let store = store(&journal, owner);
    for name in ["Docs", "bad name", "mcp__x", "workspace", "", "a__b"] {
        assert!(
            store.add(name, "https://93.184.216.34/mcp").await.is_err(),
            "{name} must be rejected"
        );
    }
    for url in [
        "http://93.184.216.34/mcp",
        "https://127.0.0.1/mcp",
        "https://10.0.0.1/mcp",
        "https://localhost/mcp",
        "https://example.local/mcp",
        "https://93.184.216.34/mcp?token=secret",
    ] {
        assert!(
            store.add("docs", url).await.is_err(),
            "{url} must be rejected"
        );
    }
    assert!(store.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn private_endpoints_are_rejected_again_before_connecting() {
    let (_root, journal, owner, _other) = fixture().await;
    // Simulate a row written before the egress rules tightened.
    sqlx::query(
        "INSERT INTO cloud_mcp_servers(principal,name,url,disabled,updated_at) VALUES(?,'legacy','https://127.0.0.1/mcp',0,0)",
    )
    .bind(owner.to_string())
    .execute(&journal.identity_pool())
    .await
    .unwrap();
    sqlx::query("INSERT INTO cloud_mcp_meta(principal,revision) VALUES(?,1)")
        .bind(owner.to_string())
        .execute(&journal.identity_pool())
        .await
        .unwrap();
    let store = store(&journal, owner);
    let cache = McpCache::new();
    assert!(cache.load(&store, owner).await.is_none());
}

#[tokio::test]
async fn an_account_without_servers_has_no_mcp_pool() {
    let (_root, journal, owner, _other) = fixture().await;
    let store = store(&journal, owner);
    let cache = McpCache::new();
    assert!(cache.load(&store, owner).await.is_none());
    assert!(cache.load(&store, owner).await.is_none());
}

#[tokio::test]
async fn the_server_limit_is_enforced() {
    let (_root, journal, owner, _other) = fixture().await;
    let store = store(&journal, owner);
    for index in 0..8 {
        store
            .add(
                &format!("s{index}"),
                &format!("https://93.184.216.{}", 34 + index),
            )
            .await
            .unwrap();
    }
    assert_eq!(
        store
            .add("s8", "https://93.184.216.50/mcp")
            .await
            .unwrap_err(),
        McpConfigError::Limit
    );
}
