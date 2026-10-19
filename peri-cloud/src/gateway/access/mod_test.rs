use super::*;
use crate::identity::ChannelIdentity;

#[test]
fn closed_default_requires_both_memberships_and_blocks_always_win() {
    let route = ChannelRoute {
        identity: ChannelIdentity {
            adapter_instance_id: "personal".into(),
            external_user_id: "user".into(),
        },
        conversation_id: "group:g".into(),
    };
    let mut policy = AccessPolicy::default();
    assert!(!policy.allows(&route, &ConversationScope::Private));
    policy.allowed_users.insert("user".into());
    assert!(policy.allows(&route, &ConversationScope::Private));
    assert!(!policy.allows(&route, &ConversationScope::Group("g".into())));
    policy.allowed_groups.insert("g".into());
    assert!(policy.allows(&route, &ConversationScope::Group("g".into())));
    policy.blocked_groups.insert("g".into());
    for mode in [AccessMode::Allowlist, AccessMode::Blocklist] {
        policy.mode = mode;
        assert!(!policy.allows(&route, &ConversationScope::Group("g".into())));
    }
    policy.blocked_users.insert("user".into());
    assert!(!policy.allows(&route, &ConversationScope::Private));
}

#[tokio::test]
async fn schema_six_upgrade_keeps_account_data_and_adds_access_tables() {
    let root = tempfile::tempdir().unwrap();
    let journal = crate::state::CloudJournal::open(root.path()).await.unwrap();
    let identity = crate::identity::IdentityService::new(
        &journal,
        "fixture-access-bootstrap-token-at-least-32-chars",
    )
    .await
    .unwrap();
    let principal = identity
        .initialize(
            "fixture-access-bootstrap-token-at-least-32-chars",
            "owner",
            "fixture-password-for-access".into(),
            "Owner",
        )
        .await
        .unwrap();
    journal
        .save_preferences(
            principal.id,
            crate::state::UserPreferences {
                revision: 0,
                instructions: "original preference".into(),
            },
        )
        .await
        .unwrap();
    let pool = journal.identity_pool();
    for statement in [
        "DROP TABLE agent_task_runs", "DROP TABLE agent_task_operations", "DROP TABLE agent_task_checkpoints",
        "DROP TABLE agent_execution_windows", "DROP TABLE agent_execution_sessions",
        "DROP TABLE agent_task_trace", "DROP TABLE agent_task_messages",
        "DROP TABLE agent_device_leases", "DROP TABLE agent_tasks",
        "DROP TABLE gateway_queue", "DROP TABLE conversation_projections",
        "DROP TABLE memory_session_audience", "DROP TABLE memory_fts",
        "DROP TABLE memory_audit",
        "DROP TABLE memory_items",
        "DROP TABLE memory_accounts",
        "CREATE TABLE user_data (principal TEXT PRIMARY KEY REFERENCES principals(id),revision INTEGER NOT NULL,instructions TEXT NOT NULL,memory TEXT NOT NULL)",
        "INSERT INTO user_data SELECT principal,revision,instructions,'' FROM user_preferences",
        "DROP TABLE user_preferences",
        "DROP TABLE scheduled_admissions",
        "DROP TABLE scheduled_tasks",
        "DROP TABLE scheduled_contact_policy",
        "DROP TABLE scheduler_occurrences",
        "DROP TABLE scheduler_plans",
        "DROP TABLE scheduler_commands",
        "DROP TABLE scheduler_metadata",
        "DROP TABLE gateway_access_inputs",
        "DROP TABLE gateway_access_audit",
        "DROP TABLE gateway_access_policies",
        "PRAGMA user_version=6",
    ] {
        sqlx::query(statement).execute(&pool).await.unwrap();
    }
    sqlx::query("UPDATE user_data SET memory='original memory'")
        .execute(&pool)
        .await
        .unwrap();
    drop(identity);
    journal.close().await;
    drop(journal);
    drop(pool);
    let journal = crate::state::CloudJournal::open(root.path()).await.unwrap();
    let (version,): (i64,) = sqlx::query_as("PRAGMA user_version")
        .fetch_one(&journal.identity_pool())
        .await
        .unwrap();
    assert_eq!(version, 19);
    assert_eq!(
        crate::memory::MemoryStore::new(&journal, principal.id, 65536)
            .list("", 0, 5)
            .await
            .unwrap()
            .items[0]
            .value
            .content,
        "original memory"
    );
    let (tables,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM sqlite_master WHERE name LIKE 'gateway_access_%' AND type='table'",
    )
    .fetch_one(&journal.identity_pool())
    .await
    .unwrap();
    assert_eq!(tables, 3);
}
