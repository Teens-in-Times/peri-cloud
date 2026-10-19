use super::*;
use crate::state::CloudJournal;
use peri_acp_types::goal::GoalStatus;
use uuid::Uuid;

async fn fixture() -> (tempfile::TempDir, CloudJournal, Uuid, Uuid, Uuid) {
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
    let session = Uuid::new_v4();
    sqlx::query("INSERT INTO cloud_sessions(id,principal,state_json) VALUES(?,?,'{}')")
        .bind(session.to_string())
        .bind(owner.to_string())
        .execute(&journal.identity_pool())
        .await
        .unwrap();
    (root, journal, owner, other, session)
}

#[tokio::test]
async fn goal_transitions_are_terminal_until_cleared() {
    let (_root, journal, owner, _other, session) = fixture().await;
    let controller = CloudGoalController::load(journal.identity_pool(), session, owner)
        .await
        .unwrap();

    assert!(controller.snapshot().status.is_none());
    controller
        .create_goal("  ship the release  ".into())
        .await
        .unwrap();
    let snapshot = controller.snapshot();
    assert_eq!(snapshot.status, Some(GoalStatus::Active));
    assert_eq!(snapshot.objective.as_deref(), Some("ship the release"));

    // The singleton slot stays occupied until clear, even by a terminal goal.
    assert!(controller.create_goal("another".into()).await.is_err());
    controller.increment_continuation().await.unwrap();
    assert_eq!(controller.snapshot().continuation_count, 1);

    controller
        .block_goal("waiting on review".into())
        .await
        .unwrap();
    let snapshot = controller.snapshot();
    assert_eq!(snapshot.status, Some(GoalStatus::Blocked));
    assert_eq!(
        snapshot.blocked_reason.as_deref(),
        Some("waiting on review")
    );
    assert!(controller.complete_goal().await.is_err());
    assert!(controller.block_goal("  ".into()).await.is_err());

    controller.clear_goal().await.unwrap();
    assert!(controller.snapshot().status.is_none());
    controller.create_goal("retry".into()).await.unwrap();
    controller.complete_goal().await.unwrap();
    assert_eq!(controller.snapshot().status, Some(GoalStatus::Complete));
}

#[tokio::test]
async fn stale_goal_completion_is_rejected_by_identity() {
    let (_root, journal, owner, _other, session) = fixture().await;
    let controller = CloudGoalController::load(journal.identity_pool(), session, owner)
        .await
        .unwrap();
    controller.create_goal("first".into()).await.unwrap();
    let first_id = controller.snapshot().goal_id.expect("goal id");

    assert!(controller.complete_goal_if_current("stale").await.is_err());
    assert_eq!(controller.snapshot().status, Some(GoalStatus::Active));

    controller.clear_goal().await.unwrap();
    controller.create_goal("second".into()).await.unwrap();
    assert!(controller
        .complete_goal_if_current(&first_id)
        .await
        .is_err());
    assert_eq!(controller.snapshot().status, Some(GoalStatus::Active));
    assert_eq!(controller.snapshot().objective.as_deref(), Some("second"));

    let second_id = controller.snapshot().goal_id.expect("goal id");
    controller
        .complete_goal_if_current(&second_id)
        .await
        .unwrap();
    assert_eq!(controller.snapshot().status, Some(GoalStatus::Complete));
}

#[tokio::test]
async fn goal_survives_reopen_and_is_scoped_to_its_principal() {
    let (root, journal, owner, other, session) = fixture().await;
    let controller = CloudGoalController::load(journal.identity_pool(), session, owner)
        .await
        .unwrap();
    controller
        .create_goal("durable objective".into())
        .await
        .unwrap();
    controller
        .block_goal("external input".into())
        .await
        .unwrap();
    journal.close().await;
    drop(controller);
    drop(journal);

    let journal = CloudJournal::open(root.path()).await.unwrap();
    let (version,): (i64,) = sqlx::query_as("PRAGMA user_version")
        .fetch_one(&journal.identity_pool())
        .await
        .unwrap();
    assert_eq!(version, 18);

    let controller = CloudGoalController::load(journal.identity_pool(), session, owner)
        .await
        .unwrap();
    let snapshot = controller.snapshot();
    assert_eq!(snapshot.objective.as_deref(), Some("durable objective"));
    assert_eq!(snapshot.status, Some(GoalStatus::Blocked));

    // Neither a different principal nor a different session inherits the goal.
    let other_controller = CloudGoalController::load(journal.identity_pool(), session, other)
        .await
        .unwrap();
    assert!(other_controller.snapshot().status.is_none());
    let unrelated = CloudGoalController::load(journal.identity_pool(), Uuid::new_v4(), owner)
        .await
        .unwrap();
    assert!(unrelated.snapshot().status.is_none());
}

#[tokio::test]
async fn another_principal_cannot_replace_a_sessions_goal() {
    let (_root, journal, owner, other, session) = fixture().await;
    let owner_controller = CloudGoalController::load(journal.identity_pool(), session, owner)
        .await
        .unwrap();
    owner_controller.create_goal("owned".into()).await.unwrap();

    let other_controller = CloudGoalController::load(journal.identity_pool(), session, other)
        .await
        .unwrap();
    assert!(other_controller.create_goal("hijack".into()).await.is_err());
    assert_eq!(
        owner_controller.snapshot().objective.as_deref(),
        Some("owned")
    );
}
