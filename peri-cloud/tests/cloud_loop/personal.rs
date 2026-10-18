use super::*;
use peri_cloud::identity::IdentityService;
use peri_cloud::limits::RuntimeLimits;
use peri_cloud::memory::{MemoryContent, MemoryKind, MemorySource, MemoryStore};
use peri_cloud::state::UserPreferences;

async fn settle(journal: &CloudJournal, principal: Uuid, session: Uuid, turn: Uuid) -> TurnState {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let state = journal.turn(principal, session, turn).await.unwrap().state;
            if !matches!(state, TurnState::Queued | TurnState::Running) {
                return state;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}
async fn owner(
    journal: &CloudJournal,
) -> (Arc<IdentityService>, peri_cloud::identity::Authenticated) {
    let identity = IdentityService::new(journal, "fixture-personal-bootstrap-32-characters")
        .await
        .unwrap();
    identity
        .initialize(
            "fixture-personal-bootstrap-32-characters",
            "owner",
            "fixture-personal-password".into(),
            "Owner",
        )
        .await
        .unwrap();
    let login = identity
        .login("owner", "fixture-personal-password".into())
        .await
        .unwrap();
    let actor = identity
        .authenticate_browser(&login.session_token)
        .await
        .unwrap();
    (identity, actor)
}

#[tokio::test]
async fn cloud_memory_uses_account_context_and_original_tool_approval() {
    for approve in [true, false] {
        let memory_id = Uuid::new_v4();
        let h = Harness::new(
            vec![
                tool_call(
                    "remember",
                    "UpdateMemory",
                    json!({"id":memory_id,"revision":1,"value":{"content":"UPDATED_OWNER_MEMORY","kind":"fact","pinned":true,"valid_until":null}}),
                ),
                answer("记忆处理完成。"),
            ],
            approve,
        )
        .await;
        let root = tempfile::tempdir().unwrap();
        let journal = Arc::new(CloudJournal::open(root.path()).await.unwrap());
        let (identity, actor) = owner(&journal).await;
        let principal = actor.principal().id;
        let invite = identity.create_invite(&actor).await.unwrap();
        let guest = identity
            .register_invited(
                &invite.code,
                "guest",
                "fixture-personal-password".into(),
                "Guest",
            )
            .await
            .unwrap();
        journal
            .save_preferences(
                principal,
                UserPreferences {
                    revision: 0,
                    instructions: "OWNER_PREFERENCE_MARKER".into(),
                },
            )
            .await
            .unwrap();
        let store = MemoryStore::new(&journal, principal, 65536);
        store
            .add(
                memory_id,
                0,
                MemoryContent {
                    content: "ORIGINAL_OWNER_MEMORY".into(),
                    kind: MemoryKind::Fact,
                    pinned: true,
                    valid_until: None,
                },
                MemorySource::web(),
            )
            .await
            .unwrap();
        let guest_store = MemoryStore::new(&journal, guest.id, 65536);
        let guest_item = guest_store
            .add(
                Uuid::new_v4(),
                0,
                MemoryContent {
                    content: "UNRELATED_GUEST_SECRET_MEMORY".into(),
                    kind: MemoryKind::Fact,
                    pinned: true,
                    valid_until: None,
                },
                MemorySource::web(),
            )
            .await
            .unwrap();
        let runtime = CloudRuntime::new(journal.clone());
        let session = runtime
            .attach(
                principal,
                h.agent.clone(),
                PermissionMode::Default,
                h.broker.clone(),
                None,
            )
            .await
            .unwrap();
        let turn = runtime
            .submit(SubmitTurn {
                principal_id: principal,
                session_id: session,
                request_key: "memory".into(),
                prompt: MessageContent::text("remember my preference"),
                max_iterations: 6,
            })
            .await
            .unwrap();
        assert_eq!(
            settle(&journal, principal, session, turn.turn_id).await,
            TurnState::Completed
        );
        let saved = store.get(memory_id).await.unwrap();
        assert_eq!(
            saved.value.content,
            if approve {
                "UPDATED_OWNER_MEMORY"
            } else {
                "ORIGINAL_OWNER_MEMORY"
            }
        );
        assert_eq!(
            journal.preferences(principal).await.unwrap().instructions,
            "OWNER_PREFERENCE_MARKER"
        );
        assert_eq!(
            guest_store.get(guest_item.id).await.unwrap().value.content,
            "UNRELATED_GUEST_SECRET_MEMORY"
        );
        let requests = serde_json::to_string(&*h.script.requests.lock()).unwrap();
        assert!(requests.contains("OWNER_PREFERENCE_MARKER"));
        assert!(!requests.contains("UNRELATED_GUEST_SECRET_MEMORY"));
        assert!(!requests.contains(&h.transport_token));
        assert!(h.broker.requests.lock().iter().any(|request|matches!(request,InteractionContext::Approval {items} if items.iter().any(|item|item.tool_name=="UpdateMemory"))));
        assert!(runtime.shutdown(Duration::from_secs(15)).await.complete);
        h.close().await;
    }
}

#[tokio::test]
async fn forgetting_in_the_agent_loop_excludes_old_tool_pairs_and_replies_after_restart() {
    let id = Uuid::new_v4();
    let h = Harness::new(
        vec![
            tool_call("recall", "GetMemory", json!({"id":id})),
            answer("Remembered: FORGOTTEN_TOOL_SECRET"),
            tool_call("forget", "ForgetMemory", json!({"id":id,"revision":1})),
            answer("已忘记。"),
            answer("新的任务。"),
        ],
        true,
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let journal = Arc::new(CloudJournal::open(root.path()).await.unwrap());
    let (_, actor) = owner(&journal).await;
    let principal = actor.principal().id;
    let store = MemoryStore::new(&journal, principal, 65536);
    store
        .add(
            id,
            0,
            MemoryContent {
                content: "FORGOTTEN_TOOL_SECRET".into(),
                kind: MemoryKind::Fact,
                pinned: false,
                valid_until: None,
            },
            MemorySource::web(),
        )
        .await
        .unwrap();
    let runtime = CloudRuntime::new(journal.clone());
    let session = runtime
        .attach(
            principal,
            h.agent.clone(),
            PermissionMode::Default,
            h.broker.clone(),
            None,
        )
        .await
        .unwrap();
    for (key, prompt) in [("recall", "read that fact"), ("forget", "forget that fact")] {
        let turn = runtime
            .submit(SubmitTurn {
                principal_id: principal,
                session_id: session,
                request_key: key.into(),
                prompt: MessageContent::text(prompt),
                max_iterations: 6,
            })
            .await
            .unwrap();
        assert_eq!(
            settle(&journal, principal, session, turn.turn_id).await,
            TurnState::Completed
        );
    }
    let before = journal.session(principal, session).await.unwrap();
    let audit = before
        .history
        .iter()
        .map(|p| peri_acp_types::store::serialize_persisted_payload(p).unwrap())
        .collect::<String>();
    assert!(audit.contains("FORGOTTEN_TOOL_SECRET"));
    assert!(audit.contains("recall"));
    {
        let requests = h.script.requests.lock();
        assert_eq!(requests.len(), 4);
        assert!(requests[1].to_string().contains("FORGOTTEN_TOOL_SECRET"));
        assert!(requests[2].to_string().contains("FORGOTTEN_TOOL_SECRET"));
        assert!(!requests[3].to_string().contains("FORGOTTEN_TOOL_SECRET"));
        assert_eq!(
            requests[0]["messages"][0], requests[3]["messages"][0],
            "base system rules stay stable"
        );
    }
    assert!(runtime.shutdown(Duration::from_secs(15)).await.complete);
    drop(runtime);
    drop(store);
    drop(journal);
    let journal = Arc::new(CloudJournal::open(root.path()).await.unwrap());
    let restored = CloudRuntime::new(journal.clone());
    restored
        .attach(
            principal,
            h.agent.clone(),
            PermissionMode::Default,
            h.broker.clone(),
            None,
        )
        .await
        .unwrap();
    let turn = restored
        .submit(SubmitTurn {
            principal_id: principal,
            session_id: session,
            request_key: "restored".into(),
            prompt: MessageContent::text("continue"),
            max_iterations: 6,
        })
        .await
        .unwrap();
    assert_eq!(
        settle(&journal, principal, session, turn.turn_id).await,
        TurnState::Completed
    );
    assert!(!h
        .script
        .requests
        .lock()
        .last()
        .unwrap()
        .to_string()
        .contains("FORGOTTEN_TOOL_SECRET"));
    assert!(restored.shutdown(Duration::from_secs(15)).await.complete);
    h.close().await;
}

#[tokio::test]
async fn daily_model_quota_blocks_a_second_loop_request_before_network_dispatch() {
    let h = Harness::new(
        vec![
            tool_call("read-memory", "SearchMemory", json!({})),
            answer("MUST_NOT_CALL"),
        ],
        true,
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let journal = Arc::new(CloudJournal::open(root.path()).await.unwrap());
    let (_, actor) = owner(&journal).await;
    let principal = actor.principal().id;
    let runtime = CloudRuntime::with_limits(
        journal.clone(),
        RuntimeLimits {
            daily_model_requests: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let session = runtime
        .attach(
            principal,
            h.agent.clone(),
            PermissionMode::Default,
            h.broker.clone(),
            None,
        )
        .await
        .unwrap();
    let turn = runtime
        .submit(SubmitTurn {
            principal_id: principal,
            session_id: session,
            request_key: "quota".into(),
            prompt: MessageContent::text("read memory"),
            max_iterations: 6,
        })
        .await
        .unwrap();
    assert_eq!(
        settle(&journal, principal, session, turn.turn_id).await,
        TurnState::Failed
    );
    assert_eq!(h.script.requests.lock().len(), 1);
    assert_eq!(
        journal.user_usage(principal).await.unwrap().model_requests,
        1
    );
    assert!(runtime.shutdown(Duration::from_secs(15)).await.complete);
    h.close().await;
}
