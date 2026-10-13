use super::*;
use peri_cloud::identity::IdentityService;
use peri_cloud::limits::RuntimeLimits;
use peri_cloud::state::UserData;

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
        let h = Harness::new(
            vec![
                tool_call(
                    "remember",
                    "WriteMemory",
                    json!({"revision":1,"memory":"UPDATED_OWNER_MEMORY"}),
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
            .save_user_data(
                principal,
                UserData {
                    revision: 0,
                    instructions: "OWNER_PREFERENCE_MARKER".into(),
                    memory: "ORIGINAL_OWNER_MEMORY".into(),
                },
                8192,
            )
            .await
            .unwrap();
        journal
            .save_user_data(
                guest.id,
                UserData {
                    memory: "UNRELATED_GUEST_SECRET_MEMORY".into(),
                    ..Default::default()
                },
                8192,
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
        let saved = journal.user_data(principal).await.unwrap();
        assert_eq!(
            saved.memory,
            if approve {
                "UPDATED_OWNER_MEMORY"
            } else {
                "ORIGINAL_OWNER_MEMORY"
            }
        );
        assert_eq!(saved.instructions, "OWNER_PREFERENCE_MARKER");
        assert_eq!(
            journal.user_data(guest.id).await.unwrap().memory,
            "UNRELATED_GUEST_SECRET_MEMORY"
        );
        let requests = serde_json::to_string(&*h.script.requests.lock()).unwrap();
        assert!(requests.contains("OWNER_PREFERENCE_MARKER"));
        assert!(!requests.contains("UNRELATED_GUEST_SECRET_MEMORY"));
        assert!(!requests.contains(&h.transport_token));
        assert!(h.broker.requests.lock().iter().any(|request|matches!(request,InteractionContext::Approval {items} if items.iter().any(|item|item.tool_name=="WriteMemory"))));
        assert!(runtime.shutdown(Duration::from_secs(15)).await.complete);
        h.close().await;
    }
}

#[tokio::test]
async fn daily_model_quota_blocks_a_second_loop_request_before_network_dispatch() {
    let h = Harness::new(
        vec![
            tool_call("read-memory", "ReadMemory", json!({})),
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
