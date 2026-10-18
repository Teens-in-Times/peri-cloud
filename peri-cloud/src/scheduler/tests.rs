use super::*;
use crate::gateway::ChannelRoute;
use crate::identity::ChannelIdentity;
use crate::state::{CloudJournal, FrozenSession};
use peri_acp_types::permission::PermissionMode;

#[test]
fn cron_requires_explicit_timezone_and_five_fields_and_handles_dst() {
    let utc = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp();
    let shanghai = Timing::Cron {
        expression: "0 14 * * *".into(),
        timezone: "Asia/Shanghai".into(),
    };
    assert_eq!(
        shanghai.next_after(utc("2026-10-18T05:59:00Z")).unwrap(),
        Some(utc("2026-10-18T06:00:00Z"))
    );
    let ny = Timing::Cron {
        expression: "0 9 * * *".into(),
        timezone: "America/New_York".into(),
    };
    assert_eq!(
        ny.next_after(utc("2026-03-07T14:00:00Z")).unwrap(),
        Some(utc("2026-03-08T13:00:00Z"))
    );
    assert_eq!(
        ny.next_after(utc("2026-10-31T13:00:00Z")).unwrap(),
        Some(utc("2026-11-01T14:00:00Z"))
    );
    for (expression, timezone) in [
        ("* * * * * *", "UTC"),
        ("61 * * * *", "UTC"),
        ("* * * * *", "server-local"),
    ] {
        assert!(Timing::Cron {
            expression: expression.into(),
            timezone: timezone.into()
        }
        .next_after(utc("2026-10-18T00:00:00Z"))
        .is_err());
    }
    assert!(Timing::Once {
        at: "2026-10-18T14:00:00".into()
    }
    .next_after(0)
    .is_err());
}

pub(super) async fn fixture() -> (tempfile::TempDir, CloudJournal, Uuid, Uuid, ChannelRoute) {
    let root = tempfile::tempdir().unwrap();
    let journal = CloudJournal::open(root.path()).await.unwrap();
    let principal = Uuid::new_v4();
    let session = Uuid::new_v4();
    let pool = journal.identity_pool();
    sqlx::query("INSERT INTO principals(id,login,display_name,password_hash) VALUES(?,'fixture','Fixture','hash')").bind(principal.to_string()).execute(&pool).await.unwrap();
    let frozen = FrozenSession {
        session_id: session,
        principal_id: principal,
        device_id: None,
        binding: None,
        system_prompt: "test".into(),
        context_window: 128000,
    };
    journal.bind(frozen, PermissionMode::Default).await.unwrap();
    let route = ChannelRoute {
        identity: ChannelIdentity {
            adapter_instance_id: "personal".into(),
            external_user_id: "12345".into(),
        },
        conversation_id: "private:12345".into(),
    };
    sqlx::query("INSERT INTO gateway_routes(route_key,route_json,principal,session_id) VALUES('fixture',?,?,?)").bind(serde_json::to_string(&route).unwrap()).bind(principal.to_string()).bind(session.to_string()).execute(&pool).await.unwrap();
    (root, journal, principal, session, route)
}
fn input(session: Uuid) -> NewSchedule {
    NewSchedule {
        mode: ScheduleMode::Conversation,
        notify: Notify::Always,
        request_id: Uuid::new_v4(),
        session_id: session,
        title: "greeting".into(),
        prompt: "hello".into(),
        timing: Timing::After { seconds: 60 },
    }
}
#[tokio::test]
async fn task_and_plan_create_atomically_and_edits_preserve_immutable_task_versions() {
    let (_root, journal, principal, session, route) = fixture().await;
    let store = ScheduleStore::new(&journal);
    let request = input(session);
    let first = store
        .create(principal, &route, request.clone(), 1000, true)
        .await
        .unwrap();
    assert_eq!(first.binding, TargetBinding::Chat);
    assert!(first.device_id.is_none());
    let repeat = store
        .create(principal, &route, request.clone(), 1010, true)
        .await
        .unwrap();
    assert_eq!(repeat.next_fire, first.next_fire);
    let mut bad = request.clone();
    bad.prompt = "changed".into();
    assert!(matches!(
        store.create(principal, &route, bad, 1010, true).await,
        Err(ScheduleError::Conflict)
    ));
    let old = store
        .core
        .get(peri_scheduler::OwnerScope(principal), first.id)
        .await
        .unwrap();
    let edit = ScheduleEdit {
        request_id: Uuid::new_v4(),
        revision: 1,
        title: "edited".into(),
        prompt: "new prompt".into(),
        timing: Timing::After { seconds: 120 },
        notify: Notify::WhenUseful,
        enabled: true,
    };
    let updated = store
        .edit(principal, first.id, edit.clone(), 1010)
        .await
        .unwrap();
    assert_eq!(updated.next_fire, Some(1130));
    assert_eq!(updated.revision, 2);
    assert_eq!(
        store
            .task(principal, &old.spec.task)
            .await
            .unwrap()
            .1
            .prompt,
        "hello"
    );
    assert_eq!(
        store
            .edit(principal, first.id, edit.clone(), 1020)
            .await
            .unwrap()
            .revision,
        2
    );
    let mut changed = edit;
    changed.prompt = "different again".into();
    assert!(matches!(
        store.edit(principal, first.id, changed, 1020).await,
        Err(ScheduleError::Conflict)
    ));
    let mut invalid = input(session);
    invalid.timing = Timing::After { seconds: 0 };
    let id = invalid.request_id;
    assert!(store
        .create(principal, &route, invalid, 1000, true)
        .await
        .is_err());
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM scheduled_tasks WHERE id=?")
        .bind(id.to_string())
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    journal.close().await;
}
#[tokio::test]
async fn contact_policy_uses_cas_and_respects_explicit_times_and_optout() {
    let (_root, journal, principal, _, _) = fixture().await;
    let store = ScheduleStore::new(&journal);
    let mut policy = store.contact_policy(principal).await.unwrap();
    policy.timezone = "UTC".into();
    policy.start_hour = 8;
    policy.end_hour = 22;
    let at = chrono::DateTime::parse_from_rfc3339("2026-10-18T02:00:00Z")
        .unwrap()
        .timestamp();
    assert!(!policy.allows(at, Notify::WhenUseful));
    assert!(policy.allows(at, Notify::Always));
    let stale = policy.clone();
    policy.enabled = false;
    let saved = store.save_contact_policy(principal, policy).await.unwrap();
    assert!(!saved.allows(at, Notify::Always));
    assert!(matches!(
        store.save_contact_policy(principal, stale).await,
        Err(ScheduleError::Conflict)
    ));
    journal.close().await;
}

#[tokio::test]
async fn scheduled_receipt_survives_current_session_change_and_charges_once() {
    let (_root, journal, principal, session, _route) = fixture().await;
    let id = Uuid::new_v4();
    let metadata = crate::state::ScheduledAdmission {
        occurrence_id: id,
        request_hash: "immutable-request".into(),
        route_json: "{}".into(),
        admitted_at: 1000,
        expires_at: 1300,
        daily_limit: 20,
    };
    let first = journal
        .admit_with_schedule(
            principal,
            session,
            "schedule:fixture".into(),
            peri_acp_types::messages::MessageContent::Text("hello".into()),
            20,
            Some(metadata.clone()),
        )
        .await
        .unwrap();
    // The route has changed: lookup must return the original turn before trying
    // to open/charge the new session (which deliberately does not exist).
    let repeat = journal
        .admit_with_schedule(
            principal,
            Uuid::new_v4(),
            "schedule:fixture".into(),
            peri_acp_types::messages::MessageContent::Text("hello".into()),
            20,
            Some(metadata.clone()),
        )
        .await
        .unwrap();
    assert!(!repeat.created);
    assert_eq!(repeat.turn.turn_id, first.turn.turn_id);
    assert_eq!(repeat.turn.session_id, session);
    assert_eq!(journal.user_usage(principal).await.unwrap().turns, 1);
    let mut conflict = metadata;
    conflict.request_hash = "different-content".into();
    assert!(matches!(
        journal
            .admit_with_schedule(
                principal,
                session,
                "schedule:fixture".into(),
                peri_acp_types::messages::MessageContent::Text("hello".into()),
                20,
                Some(conflict)
            )
            .await,
        Err(crate::state::StateError::InvocationConflict)
    ));
    assert_eq!(
        journal
            .scheduled_receipt(principal, id, "immutable-request")
            .await
            .unwrap()
            .unwrap()
            .turn_id,
        first.turn.turn_id
    );
    journal.close().await;
}
