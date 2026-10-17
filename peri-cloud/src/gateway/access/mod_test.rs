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
    let data = crate::state::UserData {
        revision: 0,
        instructions: "original preference".into(),
        memory: "original memory".into(),
    };
    journal.user_data(principal.id).await.unwrap();
    journal
        .save_user_data(principal.id, data, 8192)
        .await
        .unwrap();
    let pool = journal.identity_pool();
    for statement in [
        "DROP TABLE gateway_access_inputs",
        "DROP TABLE gateway_access_audit",
        "DROP TABLE gateway_access_policies",
        "PRAGMA user_version=6",
    ] {
        sqlx::query(statement).execute(&pool).await.unwrap();
    }
    drop(identity);
    journal.close().await;
    drop(journal);
    drop(pool);
    let journal = crate::state::CloudJournal::open(root.path()).await.unwrap();
    let (version,): (i64,) = sqlx::query_as("PRAGMA user_version")
        .fetch_one(&journal.identity_pool())
        .await
        .unwrap();
    assert_eq!(version, 8);
    assert_eq!(
        journal.user_data(principal.id).await.unwrap().memory,
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
