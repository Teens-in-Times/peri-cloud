use std::collections::HashMap;

use peri_acp_types::device_executor::{SessionBinding, WorkspaceIdentity};
use peri_acp_types::messages::{BaseMessage, MessageContent};
use peri_acp_types::permission::PermissionMode;
use peri_acp_types::store::{MessageFlags, PersistedPayload};
use peri_cloud::state::{CloudJournal, FrozenSession, StateError, TurnState};
use peri_cloud::{ChatReply, CloudTurnResult, TurnStatus};
use uuid::Uuid;

fn binding(principal: Uuid) -> FrozenSession {
    FrozenSession {
        session_id: Uuid::new_v4(),
        principal_id: principal,
        device_id: Uuid::new_v4(),
        binding: SessionBinding {
            session_id: Uuid::new_v4(),
            workspace: "/bound-device/project".into(),
            workspace_identity: WorkspaceIdentity {
                device: 1,
                inode: 2,
            },
        },
        system_prompt: "cloud frozen prompt".into(),
        context_window: 128_000,
    }
}

#[tokio::test]
async fn principal_binding_request_identity_and_exclusive_owner_are_preserved() {
    let root = tempfile::tempdir().unwrap();
    let principal = Uuid::new_v4();
    let frozen = binding(principal);
    let journal = CloudJournal::open(root.path()).await.unwrap();
    assert!(matches!(
        CloudJournal::open(root.path()).await,
        Err(StateError::AlreadyOwned)
    ));
    journal
        .bind(frozen.clone(), PermissionMode::Default)
        .await
        .unwrap();
    assert!(matches!(
        journal.session(Uuid::new_v4(), frozen.session_id).await,
        Err(StateError::Forbidden)
    ));
    let mut moved = frozen.clone();
    moved.binding.workspace_identity.inode += 1;
    assert!(matches!(
        journal.bind(moved, PermissionMode::Default).await,
        Err(StateError::BindingConflict)
    ));
    let first = journal
        .admit(
            principal,
            frozen.session_id,
            "qq:message-1".into(),
            MessageContent::text("write once"),
        )
        .await
        .unwrap();
    let repeat = journal
        .admit(
            principal,
            frozen.session_id,
            "qq:message-1".into(),
            MessageContent::text("write once"),
        )
        .await
        .unwrap();
    assert!(first.created);
    assert!(!repeat.created);
    assert_eq!(first.turn.turn_id, repeat.turn.turn_id);
    assert!(matches!(
        journal
            .admit(
                principal,
                frozen.session_id,
                "qq:message-1".into(),
                MessageContent::text("different operation")
            )
            .await,
        Err(StateError::InvocationConflict)
    ));
    assert!(matches!(
        journal
            .admit(
                principal,
                frozen.session_id,
                "qq:message-2".into(),
                MessageContent::text("second operation")
            )
            .await,
        Err(StateError::Busy)
    ));
    journal.close().await;
}

#[tokio::test]
async fn completed_turn_reply_canonical_history_and_flags_survive_reopen_without_replay() {
    let root = tempfile::tempdir().unwrap();
    let principal = Uuid::new_v4();
    let frozen = binding(principal);
    let turn_id;
    let message = BaseMessage::ai(MessageContent::text("completed reply"));
    {
        let journal = CloudJournal::open(root.path()).await.unwrap();
        journal
            .bind(frozen.clone(), PermissionMode::AcceptEdit)
            .await
            .unwrap();
        let first = journal
            .admit(
                principal,
                frozen.session_id,
                "web:stable-id".into(),
                MessageContent::text("perform task"),
            )
            .await
            .unwrap();
        turn_id = first.turn.turn_id;
        journal.start(turn_id).await.unwrap();
        let mut flags = HashMap::new();
        flags.insert(message.id(), MessageFlags::default());
        let result = CloudTurnResult {
            status: TurnStatus::Completed,
            replies: vec![ChatReply {
                message_id: message.id().as_uuid().to_string(),
                text: "completed reply".into(),
            }],
            history: vec![PersistedPayload::Message(message.clone())],
            history_flags: flags,
            internal_events: Vec::new(),
            unsettled_jobs: Some(Vec::new()),
        };
        journal.settle(turn_id, &result).await.unwrap();
        assert!(matches!(
            journal.settle(turn_id, &result).await,
            Err(StateError::TransitionConflict)
        ));
        journal.close().await;
    }
    let journal = CloudJournal::open(root.path()).await.unwrap();
    let session = journal.session(principal, frozen.session_id).await.unwrap();
    assert_eq!(session.revision, 1);
    assert_eq!(session.permissions().unwrap(), PermissionMode::AcceptEdit);
    assert_eq!(session.history[0].id(), message.id());
    assert!(session.history_flags.contains_key(&message.id()));
    let repeat = journal
        .admit(
            principal,
            frozen.session_id,
            "web:stable-id".into(),
            MessageContent::text("perform task"),
        )
        .await
        .unwrap();
    assert!(!repeat.created);
    assert_eq!(repeat.turn.turn_id, turn_id);
    assert_eq!(repeat.turn.state, TurnState::Completed);
    assert_eq!(repeat.turn.replies[0].text, "completed reply");
    let next = journal
        .admit(
            principal,
            frozen.session_id,
            "web:next-id".into(),
            MessageContent::text("next task"),
        )
        .await
        .unwrap();
    assert_eq!(next.session.history[0].id(), message.id());
    assert_eq!(next.turn.base_revision, 1);
    journal.close().await;
}

#[tokio::test]
async fn restart_separates_not_started_prompts_from_unknown_side_effects_and_never_replays() {
    let root = tempfile::tempdir().unwrap();
    let principal = Uuid::new_v4();
    let queued_session = binding(principal);
    let running_session = binding(principal);
    let (queued_id, running_id);
    {
        let journal = CloudJournal::open(root.path()).await.unwrap();
        journal
            .bind(queued_session.clone(), PermissionMode::Default)
            .await
            .unwrap();
        journal
            .bind(running_session.clone(), PermissionMode::Default)
            .await
            .unwrap();
        queued_id = journal
            .admit(
                principal,
                queued_session.session_id,
                "queued".into(),
                MessageContent::text("queued task"),
            )
            .await
            .unwrap()
            .turn
            .turn_id;
        running_id = journal
            .admit(
                principal,
                running_session.session_id,
                "running".into(),
                MessageContent::text("may have executed"),
            )
            .await
            .unwrap()
            .turn
            .turn_id;
        journal.start(running_id).await.unwrap();
        journal.close().await;
    }
    let journal = CloudJournal::open(root.path()).await.unwrap();
    assert_eq!(
        journal
            .turn(principal, queued_session.session_id, queued_id)
            .await
            .unwrap()
            .state,
        TurnState::Interrupted
    );
    assert_eq!(
        journal
            .turn(principal, running_session.session_id, running_id)
            .await
            .unwrap()
            .state,
        TurnState::RecoveryRequired
    );
    assert!(!journal
        .session(principal, running_session.session_id)
        .await
        .unwrap()
        .execution_settled());
    let repeat = journal
        .admit(
            principal,
            running_session.session_id,
            "running".into(),
            MessageContent::text("may have executed"),
        )
        .await
        .unwrap();
    assert!(!repeat.created);
    assert_eq!(repeat.turn.turn_id, running_id);
    journal
        .reconcile_jobs(principal, running_session.session_id, 0, Vec::new())
        .await
        .unwrap();
    assert!(matches!(
        journal
            .admit(
                principal,
                running_session.session_id,
                "new".into(),
                MessageContent::text("new work")
            )
            .await,
        Err(StateError::Busy)
    ));
    journal
        .admit(
            principal,
            queued_session.session_id,
            "new".into(),
            MessageContent::text("new work"),
        )
        .await
        .unwrap();
    journal.close().await;
}

#[tokio::test]
async fn unconfirmed_executor_state_blocks_new_turn_after_model_loop_finishes() {
    let root = tempfile::tempdir().unwrap();
    let principal = Uuid::new_v4();
    let frozen = binding(principal);
    let journal = CloudJournal::open(root.path()).await.unwrap();
    journal
        .bind(frozen.clone(), PermissionMode::Default)
        .await
        .unwrap();
    let turn = journal
        .admit(
            principal,
            frozen.session_id,
            "one".into(),
            MessageContent::text("task"),
        )
        .await
        .unwrap()
        .turn;
    journal.start(turn.turn_id).await.unwrap();
    let result = CloudTurnResult {
        status: TurnStatus::Completed,
        replies: Vec::new(),
        history: Vec::new(),
        history_flags: HashMap::new(),
        internal_events: Vec::new(),
        unsettled_jobs: None,
    };
    journal.settle(turn.turn_id, &result).await.unwrap();
    assert!(matches!(
        journal
            .admit(
                principal,
                frozen.session_id,
                "two".into(),
                MessageContent::text("new task")
            )
            .await,
        Err(StateError::Busy)
    ));
    journal
        .reconcile_jobs(principal, frozen.session_id, 1, Vec::new())
        .await
        .unwrap();
    journal
        .admit(
            principal,
            frozen.session_id,
            "two".into(),
            MessageContent::text("new task"),
        )
        .await
        .unwrap();
    journal.close().await;
}
