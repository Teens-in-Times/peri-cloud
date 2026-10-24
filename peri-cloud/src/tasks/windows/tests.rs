use super::*;
use crate::state::{CloudJournal, FrozenSession};
use peri_acp_types::permission::PermissionMode;
use serde_json::json;
use std::sync::Arc;

async fn fixture() -> (tempfile::TempDir, Arc<WorkerService>, TaskRecord) {
    let root = tempfile::tempdir().unwrap();
    let journal = Arc::new(CloudJournal::open(root.path()).await.unwrap());
    let owner = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO principals(id,login,display_name,password_hash) VALUES(?,?,?,'fixture')",
    )
    .bind(owner.to_string())
    .bind(owner.to_string())
    .bind("window fixture")
    .execute(&journal.identity_pool())
    .await
    .unwrap();
    let parent = Uuid::new_v4();
    let window = Uuid::new_v4();
    for id in [parent, window] {
        journal
            .bind(
                FrozenSession {
                    session_id: id,
                    principal_id: owner,
                    device_id: None,
                    binding: None,
                    system_prompt: "fixture".into(),
                    context_window: 48000,
                },
                PermissionMode::Default,
            )
            .await
            .unwrap();
    }
    let record:TaskRecord=serde_json::from_value(json!({"id":Uuid::new_v4(),"principal":owner,"parent_session":parent,"worker_session":window,
        "execution_session":Uuid::new_v4(),"parent_turn":Uuid::new_v4(),"request_key":"fixture","request_fingerprint":"fixture",
        "route":{"identity":{"adapter_instance_id":"fixture","external_user_id":"owner"},"conversation_id":"private"},
        "goal":"test durable window lifecycle","reply_to":"","reply_kind":"message","constraints":"preserve scope","acceptance":"evidence",
        "state":"running","turn":Uuid::new_v4(),"revision":0,"note":"","plan":[],"result":"","notified":false,
        "privacy_epoch":0,"parent_generation":0,"permission_mode":0,"private_memory":true,"created_at":now()})).unwrap();
    let service = WorkerService::new(Weak::new(), journal);
    let turn = crate::state::TurnRecord {
        turn_id: record.turn.unwrap(),
        session_id: window,
        request_key: "fixture-source".into(),
        prompt: peri_acp_types::messages::MessageContent::text("source work"),
        permission_mode: 0,
        base_revision: 0,
        state: crate::state::TurnState::Completed,
        replies: Vec::new(),
        cancel_requested: false,
    };
    sqlx::query("INSERT INTO cloud_turns VALUES(?,?,?,'completed',?)")
        .bind(turn.turn_id.to_string())
        .bind(window.to_string())
        .bind(&turn.request_key)
        .bind(serde_json::to_string(&turn).unwrap())
        .execute(&service.store.pool)
        .await
        .unwrap();
    service.store.insert(&record).await.unwrap();
    (root, service, record)
}

#[tokio::test]
async fn rotation_and_task_control_commit_together_and_stale_revision_cannot_add_an_intent() {
    let (_root, service, mut record) = fixture().await;
    let mut stale = record.clone();
    service
        .begin_rotation(&mut record, RotationCause::Manual)
        .await
        .unwrap();
    assert_eq!(record.state, "rotate_requested");
    assert!(record.manual_goal_guard);
    let first = service
        .rotation(record.id, record.worker_session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.prior_state, "running");
    service
        .begin_rotation(&mut record, RotationCause::Manual)
        .await
        .unwrap();
    assert_eq!(
        service
            .rotation(record.id, record.worker_session)
            .await
            .unwrap()
            .unwrap()
            .id,
        first.id
    );
    assert!(matches!(
        service
            .begin_rotation(&mut stale, RotationCause::Worker)
            .await,
        Err(TaskError::Conflict)
    ));
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_window_rotations")
        .fetch_one(&service.store.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn restart_retains_unknown_summary_identity_as_fallback_without_a_new_attempt() {
    let (root, service, mut record) = fixture().await;
    service
        .begin_rotation(&mut record, RotationCause::Worker)
        .await
        .unwrap();
    let mut rotation = service
        .rotation(record.id, record.worker_session)
        .await
        .unwrap()
        .unwrap();
    rotation.phase = RotationPhase::Summarizing;
    rotation.summary_status = "submitted_or_unknown".into();
    service
        .save_rotation(&rotation, RotationPhase::Requested)
        .await
        .unwrap();
    service.journal.close().await;
    drop(service);
    let journal = Arc::new(CloudJournal::open(root.path()).await.unwrap());
    let service = WorkerService::new(Weak::new(), journal);
    let restored = service
        .rotation(record.id, record.worker_session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(restored.id, rotation.id);
    assert_eq!(restored.phase, RotationPhase::Prepared);
    assert_eq!(restored.summary_status, "unknown_after_restart");
    assert!(restored.summary.is_none());
    assert_eq!(
        service
            .request_rotation(&record, RotationCause::Budget)
            .await
            .unwrap()
            .id,
        rotation.id
    );
}

#[tokio::test]
async fn provider_usage_calibrates_total_input_and_fresh_overflow_waits_instead_of_spinning() {
    use peri_model::{ModelMessage, ModelResponse, ProviderProtocol, StopReason, TokenUsage};
    let (_root, service, record) = fixture().await;
    assert!(!budget::preflight(
        &service.store.pool,
        record.principal,
        record.worker_session,
        1000,
        512,
        64
    )
    .await
    .unwrap());
    let mut current = service
        .store
        .get(record.principal, record.id)
        .await
        .unwrap();
    assert_eq!(current.state, "waiting_external");
    let usage = TokenUsage {
        input_tokens: 50,
        output_tokens: 20,
        cache_creation_input_tokens: Some(200),
        cache_read_input_tokens: Some(300),
    };
    let response = ModelResponse::new(
        ModelMessage::assistant_text("complete"),
        StopReason::EndTurn,
        Some(usage),
        None,
    )
    .unwrap();
    budget::record(
        &service.store.pool,
        record.principal,
        record.worker_session,
        100,
        40,
        &response,
        Some(ProviderProtocol::Anthropic),
    )
    .await
    .unwrap();
    let observation =
        budget::observation(&service.store.pool, record.principal, record.worker_session)
            .await
            .unwrap();
    assert_eq!(observation.observed_input, Some(550));
    assert_eq!(budget::calibrate(100, &observation), 550);
    current.state = "running".into();
    service.store.save(&mut current).await.unwrap();
    assert!(!budget::preflight(
        &service.store.pool,
        record.principal,
        record.worker_session,
        100,
        600,
        100
    )
    .await
    .unwrap());
    assert_eq!(
        service
            .store
            .get(record.principal, record.id)
            .await
            .unwrap()
            .state,
        "rotate_requested"
    );
}

#[tokio::test]
async fn evidence_search_advances_empty_pages_and_details_stay_bound_to_the_task() {
    let (_root, service, record) = fixture().await;
    for n in 0..70 {
        let call = json!({"id":format!("call-{n}"),"name":"Read","input":{"file_path":if n==69{"needle.txt"}else{"other.txt"}}});
        sqlx::query("INSERT INTO agent_task_operations(task_id,turn_id,window_id,call_id,call_hash,call_json) VALUES(?,?,?,?,?,?)")
            .bind(record.id.to_string()).bind(record.turn.unwrap().to_string()).bind(record.worker_session.to_string())
            .bind(format!("call-{n}")).bind(format!("hash-{n}")).bind(call.to_string()).execute(&service.store.pool).await.unwrap();
    }
    let first = service
        .search_history(
            &record,
            None,
            0,
            4,
            Some("needle"),
            HistoryFilter::default(),
        )
        .await
        .unwrap();
    assert!(first["entries"].as_array().unwrap().is_empty());
    assert_eq!(first["has_more"], true);
    let last = service
        .search_history(
            &record,
            None,
            first["next_cursor"].as_i64().unwrap(),
            4,
            Some("needle"),
            HistoryFilter::default(),
        )
        .await
        .unwrap();
    assert_eq!(last["entries"].as_array().unwrap().len(), 1);
    assert_eq!(last["has_more"], false);
    let detail: super::super::evidence::Detail =
        serde_json::from_value(last["entries"][0]["input_detail"].clone()).unwrap();
    let body = service.detail(&record, None, detail).await.unwrap();
    assert!(body["text"].as_str().unwrap().contains("needle.txt"));
    let mut other = record.clone();
    other.id = Uuid::new_v4();
    let detail = serde_json::from_value(last["entries"][0]["input_detail"].clone()).unwrap();
    assert!(matches!(
        service.detail(&other, None, detail).await,
        Err(TaskError::Forbidden)
    ));
}
