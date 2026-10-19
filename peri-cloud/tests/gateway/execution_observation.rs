use super::*;

async fn observation(f: &Fixture, session: Uuid) -> (Uuid, i64) {
    let state = f
        .c()
        .journal
        .session(f.c().browser.principal().id, session)
        .await
        .unwrap();
    state
        .history
        .iter()
        .rev()
        .find_map(|p| {
            if state
                .history_flags
                .get(&p.id())
                .is_some_and(|v| v.excluded || v.truncated)
            {
                return None;
            }
            match p {
                peri_acp_types::store::PersistedPayload::SystemReminder { reminder, .. }
                    if reminder.as_reminder().source.0 == "cloud_tasks" =>
                {
                    reminder.as_reminder().metadata["conversation_turn_cursor"]
                        .as_i64()
                        .map(|cursor| (p.id().as_uuid(), cursor))
                }
                _ => None,
            }
        })
        .unwrap()
}

#[tokio::test]
async fn periodic_task_refresh_counts_conversation_rounds_and_survives_restart() {
    let limits = peri_cloud::limits::RuntimeLimits {
        task_refresh_turns: 2,
        ..Default::default()
    };
    let mut f = Fixture::new_for_limits(
        vec![
            answer("first"),
            answer("other conversation"),
            call("list_tasks", json!({}), "task-read"),
            answer("second with tools"),
            answer("third"),
        ],
        true,
        limits,
    )
    .await;
    let parent = f
        .c()
        .gateway
        .open_browser_workspace(&f.c().browser, Uuid::new_v4(), f.devices[0].id, None)
        .await
        .unwrap();
    let other = f
        .c()
        .gateway
        .open_browser_workspace(&f.c().browser, Uuid::new_v4(), f.devices[1].id, None)
        .await
        .unwrap();
    let turn = f
        .c()
        .gateway
        .submit_browser_task(&f.c().browser, parent, Uuid::new_v4(), "first".into())
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    let first = observation(&f, parent).await;
    let turn = f
        .c()
        .gateway
        .submit_browser_task(&f.c().browser, other, Uuid::new_v4(), "unrelated".into())
        .await
        .unwrap();
    f.settled(other, turn.turn_id).await;
    let turn = f
        .c()
        .gateway
        .submit_browser_task(&f.c().browser, parent, Uuid::new_v4(), "second".into())
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    assert_eq!(
        observation(&f, parent).await,
        first,
        "other conversations and tool iterations do not advance this conversation's interval"
    );
    let turn = f
        .c()
        .gateway
        .submit_browser_task(&f.c().browser, parent, Uuid::new_v4(), "third".into())
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    let refreshed = observation(&f, parent).await;
    assert_ne!(refreshed.0, first.0);
    assert!(refreshed.1 > first.1);
    f.restart().await;
    f.script.replies.lock().push_back(answer("fourth"));
    let turn = f
        .c()
        .gateway
        .submit_browser_task(&f.c().browser, parent, Uuid::new_v4(), "fourth".into())
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    assert_eq!(
        observation(&f, parent).await,
        refreshed,
        "durable cursor prevents redundant refresh on restart"
    );
}
