use super::*;
use crate::state::CloudJournal;
use peri_acp_types::{
    device_executor::{SessionBinding, WorkspaceIdentity},
    permission::PermissionMode,
};
use peri_scheduler::{DispatchState, OwnerScope};
use sqlx::{sqlite::SqliteConnectOptions, Row};

#[tokio::test]
async fn legacy_plan_identity_receipts_and_unknowns_survive_migration_without_broadening_target() {
    let (root, journal, principal, _, route) = super::tests::fixture().await;
    let session = Uuid::new_v4();
    let device = Uuid::new_v4();
    let id = Uuid::new_v4();
    let planned = 1_900_000_000;
    let request_key = format!("schedule:{id}:{planned}");
    journal
        .bind(
            crate::state::FrozenSession {
                session_id: session,
                principal_id: principal,
                device_id: Some(device),
                binding: Some(SessionBinding {
                    session_id: Uuid::new_v4(),
                    workspace: "/old".into(),
                    workspace_identity: WorkspaceIdentity {
                        device: 1,
                        inode: 2,
                    },
                }),
                system_prompt: "fixture".into(),
                context_window: 128000,
            },
            PermissionMode::Default,
        )
        .await
        .unwrap();
    let turn = journal
        .admit(
            principal,
            session,
            request_key.clone(),
            peri_acp_types::messages::MessageContent::Text("old".into()),
        )
        .await
        .unwrap()
        .turn;
    journal.close().await;
    drop(journal);
    let pool = sqlx::SqlitePool::connect_with(
        SqliteConnectOptions::new().filename(root.path().join("cloud.sqlite")),
    )
    .await
    .unwrap();
    for sql in [
        "DROP TABLE agent_task_approval_presentations", "DROP TABLE cloud_native_cancellations", "DROP TABLE cloud_native_operations", "DROP TABLE cloud_device_targets", "DROP TABLE agent_task_instruction_reads", "DROP TABLE agent_task_approvals", "DROP TABLE agent_task_runs", "DROP TABLE agent_task_operations", "DROP TABLE agent_task_checkpoints",
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
        "DROP TABLE scheduled_admissions","DROP TABLE scheduled_tasks","DROP TABLE scheduled_contact_policy",
        "DROP TABLE scheduler_occurrences","DROP TABLE scheduler_plans","DROP TABLE scheduler_commands","DROP TABLE scheduler_metadata",
        "CREATE TABLE cloud_schedules(id TEXT PRIMARY KEY,principal TEXT,session_id TEXT,route_json TEXT,request_json TEXT,record_json TEXT,enabled INTEGER,deleted INTEGER,revision INTEGER,next_fire INTEGER)",
        "CREATE TABLE cloud_schedule_runs(schedule_id TEXT,scheduled_at INTEGER,request_key TEXT,state TEXT,delivery_state TEXT,reason TEXT,turn_id TEXT)",
        "PRAGMA user_version=10",
    ] {sqlx::query(sql).execute(&pool).await.unwrap();}
    let task = Schedule {
        id,
        session_id: session,
        device_id: Some(device),
        workspace: "/old".into(),
        mode: ScheduleMode::Conversation,
        notify: Notify::Always,
        title: "legacy".into(),
        prompt: "hello".into(),
        timing: Timing::Cron {
            expression: "* * * * *".into(),
            timezone: "UTC".into(),
        },
        enabled: true,
        deleted: false,
        revision: 9,
        next_fire: Some(planned + 60),
        created_at: planned - 60,
        notification: "push".into(),
        binding: TargetBinding::LegacyFrozenSession,
    };
    let mut old_json = serde_json::to_value(&task).unwrap();
    old_json.as_object_mut().unwrap().remove("binding");
    sqlx::query("INSERT INTO cloud_schedules VALUES(?,?,?,?,?,?,1,0,9,?)")
        .bind(id.to_string())
        .bind(principal.to_string())
        .bind(session.to_string())
        .bind(serde_json::to_string(&route).unwrap())
        .bind("{}")
        .bind(old_json.to_string())
        .bind(planned + 60)
        .execute(&pool)
        .await
        .unwrap();
    // Lost turn ID is recovered from the original session/request key.
    sqlx::query("INSERT INTO cloud_schedule_runs VALUES(?,?,?,'claimed','pending',NULL,NULL)")
        .bind(id.to_string())
        .bind(planned)
        .bind(&request_key)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO cloud_schedule_runs VALUES(?,?,?,'recovery_required','unknown','old_unknown',NULL)").bind(id.to_string()).bind(planned-60).bind(format!("schedule:{id}:{}",planned-60)).execute(&pool).await.unwrap();
    pool.close().await;
    let journal = CloudJournal::open(root.path()).await.unwrap();
    let store = ScheduleStore::new(&journal);
    let migrated = store.get(principal, id).await.unwrap();
    assert_eq!(migrated.binding, TargetBinding::LegacyFrozenSession);
    assert_eq!(migrated.device_id, Some(device));
    assert_eq!(migrated.session_id, session);
    assert_eq!(migrated.workspace, "/old");
    assert_eq!(migrated.revision, 9);
    let events = store
        .core
        .occurrences(OwnerScope(principal), id, 0, 100)
        .await
        .unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0].run_ref.as_deref(),
        Some(turn.turn_id.to_string().as_str())
    );
    assert_eq!(events[1].state, DispatchState::Unknown);
    let row = sqlx::query(
        "SELECT delivery_state,turn_id FROM scheduled_admissions WHERE occurrence_id=?",
    )
    .bind(events[0].id.to_string())
    .fetch_one(&store.pool)
    .await
    .unwrap();
    assert_eq!(
        row.get::<String, _>("delivery_state"),
        "suppressed",
        "old pending deliveries lack a verifiable context generation"
    );
    assert_eq!(row.get::<String, _>("turn_id"), turn.turn_id.to_string());
    let (version,): (i64,) = sqlx::query_as("PRAGMA user_version")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert_eq!(version, 24);
    journal.close().await;
    drop(journal);
    let journal = CloudJournal::open(root.path()).await.unwrap();
    assert_eq!(
        ScheduleStore::new(&journal)
            .runs(principal, id)
            .await
            .unwrap()
            .len(),
        2
    );
    journal.close().await;
}
