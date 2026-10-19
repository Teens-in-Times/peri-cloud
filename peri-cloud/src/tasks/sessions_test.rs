use super::*;
use crate::identity::ChannelIdentity;
use peri_acp_types::device_executor::{SessionBinding, WorkspaceIdentity};
use peri_acp_types::permission::PermissionMode;
use std::sync::Arc;

fn frozen(owner: Uuid, device: Option<Uuid>, workspace: &str) -> FrozenSession {
    FrozenSession {
        session_id: Uuid::new_v4(),
        principal_id: owner,
        device_id: device,
        binding: device.map(|_| SessionBinding {
            session_id: Uuid::new_v4(),
            workspace: workspace.into(),
            workspace_identity: WorkspaceIdentity {
                device: 1,
                inode: 2,
            },
        }),
        system_prompt: "fixture".into(),
        context_window: 128000,
    }
}

async fn fixture() -> (
    tempfile::TempDir,
    Arc<WorkerService>,
    TaskScope,
    FrozenSession,
) {
    let root = tempfile::tempdir().unwrap();
    let journal = Arc::new(crate::state::CloudJournal::open(root.path()).await.unwrap());
    let owner = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO principals(id,login,display_name,password_hash) VALUES(?,?,?,'fixture')",
    )
    .bind(owner.to_string())
    .bind(owner.to_string())
    .bind("fixture")
    .execute(&journal.identity_pool())
    .await
    .unwrap();
    let parent = frozen(owner, None, "");
    journal
        .bind(parent.clone(), PermissionMode::Default)
        .await
        .unwrap();
    let worker = frozen(owner, Some(Uuid::new_v4()), "/canonical/project");
    journal
        .bind(worker.clone(), PermissionMode::Default)
        .await
        .unwrap();
    let service = WorkerService::new(Weak::new(), journal);
    let scope = TaskScope {
        service: Arc::downgrade(&service),
        principal: owner,
        session: parent.session_id,
        root_session: parent.session_id,
        task: None,
        role: AgentRole::Main,
        execution_agent: None,
        route: ChannelRoute {
            identity: ChannelIdentity {
                adapter_instance_id: "fixture".into(),
                external_user_id: "owner".into(),
            },
            conversation_id: "private".into(),
        },
    };
    (root, service, scope, worker)
}

fn task(scope: &TaskScope, execution: &ExecutionSession, key: &str) -> TaskRecord {
    TaskRecord {
        id: Uuid::new_v4(),
        principal: scope.principal,
        parent_session: scope.session,
        worker_session: execution.window,
        execution_session: execution.id,
        parent_turn: Uuid::new_v4(),
        request_key: key.into(),
        request_fingerprint: key.into(),
        route: scope.route.clone(),
        goal: key.into(),
        reply_to: String::new(),
        reply_kind: crate::gateway::ReplyKind::Message,
        constraints: String::new(),
        acceptance: String::new(),
        state: "queued".into(),
        turn: None,
        revision: 0,
        note: String::new(),
        plan: Vec::new(),
        result: String::new(),
        notified: false,
        privacy_epoch: 0,
        parent_generation: 0,
        permission_mode: 0,
        private_memory: true,
        created_at: now(),
    }
}

#[tokio::test]
async fn multiple_tasks_share_context_but_serialize_and_keep_independent_records() {
    let (_root, service, scope, worker) = fixture().await;
    let execution = service
        .resolve_execution_session(&scope, None, &worker, SessionBoundary::new(true, 0, 0))
        .await
        .unwrap();
    let mut first = task(&scope, &execution, "first");
    let second = task(&scope, &execution, "second");
    service.store.insert(&first).await.unwrap();
    service.store.insert(&second).await.unwrap();
    assert!(service.session_head(&first).await.unwrap());
    assert!(!service.session_head(&second).await.unwrap());
    assert_eq!(
        service
            .store
            .by_session(scope.principal, worker.session_id)
            .await
            .unwrap()
            .unwrap()
            .id,
        first.id
    );
    first.note = "first task evidence".into();
    first.state = "completed".into();
    service.store.save(&mut first).await.unwrap();
    assert!(service.session_head(&second).await.unwrap());
    assert_eq!(
        service
            .store
            .by_session(scope.principal, worker.session_id)
            .await
            .unwrap()
            .unwrap()
            .id,
        second.id
    );
    assert!(service
        .store
        .get(scope.principal, second.id)
        .await
        .unwrap()
        .note
        .is_empty());
    let same = service
        .resolve_execution_session(
            &scope,
            Some(execution.id),
            &worker,
            SessionBoundary::new(true, 0, 0),
        )
        .await
        .unwrap();
    assert_eq!(same.agent_id, execution.agent_id);
    assert_eq!(same.window, worker.session_id);
    assert!(matches!(
        service.store.get(Uuid::new_v4(), first.id).await,
        Err(TaskError::Forbidden)
    ));
}

#[tokio::test]
async fn explicit_reuse_rejects_changed_identity_workspace_privacy_and_forgotten_sources() {
    let (_root, service, scope, worker) = fixture().await;
    let execution = service
        .resolve_execution_session(&scope, None, &worker, SessionBoundary::new(true, 0, 0))
        .await
        .unwrap();
    let mut changed = worker.clone();
    changed.binding.as_mut().unwrap().workspace = "/other".into();
    assert!(matches!(
        service
            .resolve_execution_session(
                &scope,
                Some(execution.id),
                &changed,
                SessionBoundary::new(true, 0, 0)
            )
            .await,
        Err(TaskError::Forbidden)
    ));
    changed = worker.clone();
    changed.binding.as_mut().unwrap().workspace_identity.inode += 1;
    assert!(matches!(
        service
            .resolve_execution_session(
                &scope,
                Some(execution.id),
                &changed,
                SessionBoundary::new(true, 0, 0)
            )
            .await,
        Err(TaskError::Forbidden)
    ));
    changed = worker.clone();
    changed.device_id = Some(Uuid::new_v4());
    assert!(matches!(
        service
            .resolve_execution_session(
                &scope,
                Some(execution.id),
                &changed,
                SessionBoundary::new(true, 0, 0)
            )
            .await,
        Err(TaskError::Forbidden)
    ));
    assert!(matches!(
        service
            .resolve_execution_session(
                &scope,
                Some(execution.id),
                &worker,
                SessionBoundary::new(false, 0, 0)
            )
            .await,
        Err(TaskError::Forbidden)
    ));
    assert!(matches!(
        service
            .resolve_execution_session(
                &scope,
                Some(execution.id),
                &worker,
                SessionBoundary::new(true, 1, 0)
            )
            .await,
        Err(TaskError::Forbidden)
    ));
    let mut other = scope.clone();
    other.principal = Uuid::new_v4();
    assert!(matches!(
        service
            .resolve_execution_session(
                &other,
                Some(execution.id),
                &worker,
                SessionBoundary::new(true, 0, 0)
            )
            .await,
        Err(TaskError::Forbidden)
    ));
    sqlx::query("UPDATE cloud_sessions SET state_json=json_set(state_json,'$.context_generation',1) WHERE id=?")
        .bind(scope.session.to_string()).execute(&service.store.pool).await.unwrap();
    assert!(matches!(
        service
            .resolve_execution_session(
                &scope,
                Some(execution.id),
                &worker,
                SessionBoundary::new(true, 0, 1)
            )
            .await,
        Err(TaskError::Forbidden)
    ));
}

#[tokio::test]
async fn explicit_reuse_from_another_main_session_tracks_both_source_generations() {
    let (_root, service, scope, worker) = fixture().await;
    let execution = service
        .resolve_execution_session(&scope, None, &worker, SessionBoundary::new(true, 0, 0))
        .await
        .unwrap();
    let next_parent = frozen(scope.principal, None, "");
    service
        .journal
        .bind(next_parent.clone(), PermissionMode::Default)
        .await
        .unwrap();
    let mut next = scope.clone();
    next.session = next_parent.session_id;
    next.root_session = next.session;
    let reused = service
        .resolve_execution_session(
            &next,
            Some(execution.id),
            &worker,
            SessionBoundary::new(true, 0, 0),
        )
        .await
        .unwrap();
    assert_eq!(reused.sources.len(), 2);
    assert_eq!(reused.id, execution.id);
    let record = task(&next, &reused, "next");
    assert!(service.execution_valid(&record).await.unwrap());
    sqlx::query("UPDATE cloud_sessions SET state_json=json_set(state_json,'$.context_generation',1) WHERE id=?")
        .bind(scope.session.to_string()).execute(&service.store.pool).await.unwrap();
    assert!(!service.execution_valid(&record).await.unwrap());
}

#[tokio::test]
async fn public_execution_context_is_scoped_to_its_original_audience() {
    let (_root, service, mut scope, worker) = fixture().await;
    scope.route.conversation_id = "group:first".into();
    let execution = service
        .resolve_execution_session(&scope, None, &worker, SessionBoundary::new(false, 0, 0))
        .await
        .unwrap();
    assert!(service
        .resolve_execution_session(
            &scope,
            Some(execution.id),
            &worker,
            SessionBoundary::new(false, 0, 0)
        )
        .await
        .is_ok());
    scope.route.conversation_id = "group:second".into();
    assert!(matches!(
        service
            .resolve_execution_session(
                &scope,
                Some(execution.id),
                &worker,
                SessionBoundary::new(false, 0, 0)
            )
            .await,
        Err(TaskError::Forbidden)
    ));
}

#[tokio::test]
async fn schema_rebuild_preserves_old_task_and_unknown_operation_identity() {
    let (root, service, scope, worker) = fixture().await;
    let execution = service
        .resolve_execution_session(&scope, None, &worker, SessionBoundary::new(true, 0, 0))
        .await
        .unwrap();
    let mut original = task(&scope, &execution, "legacy");
    original.state = "recovery_required".into();
    original.turn = Some(Uuid::new_v4());
    original.result = "unknown operation; do not replay".into();
    service.store.insert(&original).await.unwrap();
    for sql in [
        "DROP TABLE agent_task_operations",
        "DROP TABLE agent_task_checkpoints",
        "DROP TABLE agent_execution_windows",
        "DROP TABLE agent_execution_sessions",
        "CREATE UNIQUE INDEX legacy_one_worker ON agent_tasks(worker_session)",
        "PRAGMA user_version=16",
    ] {
        sqlx::query(sql).execute(&service.store.pool).await.unwrap();
    }
    service.journal.close().await;
    drop(service);
    let journal = crate::state::CloudJournal::open(root.path()).await.unwrap();
    let store = super::super::store::TaskStore {
        pool: journal.identity_pool(),
    };
    let restored = store.get(scope.principal, original.id).await.unwrap();
    assert_eq!(restored.turn, original.turn);
    assert_eq!(restored.worker_session, original.worker_session);
    assert_eq!(restored.state, "recovery_required");
    assert_eq!(restored.result, original.result);
    let mappings: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_execution_windows WHERE execution_session=? AND cloud_session=?")
        .bind(restored.execution_session.to_string()).bind(original.worker_session.to_string()).fetch_one(&store.pool).await.unwrap();
    assert_eq!(mappings, 1);
    let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert_eq!(fk, 1);
    assert!(sqlx::query("PRAGMA foreign_key_check")
        .fetch_optional(&store.pool)
        .await
        .unwrap()
        .is_none());
}
