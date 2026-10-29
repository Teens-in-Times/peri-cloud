use super::*;
use peri_cloud::gateway::{BookmarkCommand, GatewayError};

#[tokio::test]
async fn fleet_bookmarks_are_device_scoped_versioned_and_persist_without_execution() {
    let mut f = Fixture::new_for(vec![], true).await;
    let device = f.devices[0].id;
    let initial = f
        .c()
        .gateway
        .browser_device_management(&f.c().browser)
        .await
        .unwrap();
    assert!(initial[&device].check.is_none());
    assert!(initial[&device].recommendation.is_none());
    let checked = f
        .c()
        .gateway
        .browser_inspect_device(&f.c().browser, device)
        .await
        .unwrap();
    assert!(checked.authenticated);
    assert!(checked
        .details
        .unwrap()
        .tools
        .unwrap()
        .iter()
        .any(|t| t == "Write"));
    let current = f
        .c()
        .gateway
        .browser_device_management(&f.c().browser)
        .await
        .unwrap();
    assert!(current[&device].recommendation.is_some());
    f.clock.0.fetch_add(61, Ordering::SeqCst);
    let stale = f
        .c()
        .gateway
        .browser_device_management(&f.c().browser)
        .await
        .unwrap();
    assert!(!stale[&device].check.as_ref().unwrap().fresh);
    assert!(stale[&device].recommendation.is_none());

    let path = f.workspaces[0].path().to_string_lossy().into_owned();
    let saved = f
        .c()
        .gateway
        .browser_bookmark(
            &f.c().browser,
            device,
            BookmarkCommand::Save {
                workspace: path.clone(),
                label: "project".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(saved.len(), 1);
    let bookmark = saved[0].clone();
    let repeated = f
        .c()
        .gateway
        .browser_bookmark(
            &f.c().browser,
            device,
            BookmarkCommand::Save {
                workspace: path,
                label: "not a silent rename".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(repeated[0].id, bookmark.id);
    assert_eq!(repeated[0].label, "project");
    assert_eq!(repeated[0].revision, bookmark.revision);
    let renamed = f
        .c()
        .gateway
        .browser_bookmark(
            &f.c().browser,
            device,
            BookmarkCommand::Rename {
                id: bookmark.id,
                revision: bookmark.revision,
                label: "renamed".into(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        f.c()
            .gateway
            .browser_bookmark(
                &f.c().browser,
                device,
                BookmarkCommand::Remove {
                    id: bookmark.id,
                    revision: bookmark.revision
                }
            )
            .await,
        Err(GatewayError::BookmarkConflict)
    ));
    assert!(matches!(
        f.c()
            .gateway
            .browser_bookmark(
                &f.c().browser,
                f.devices[1].id,
                BookmarkCommand::Remove {
                    id: bookmark.id,
                    revision: renamed[0].revision
                }
            )
            .await,
        Err(GatewayError::BookmarkConflict)
    ));
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(f.root.path().join("cloud.sqlite")),
        )
        .await
        .unwrap();
    let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cloud_sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        sessions, 0,
        "management must not bind sessions or admit tasks"
    );
    pool.close().await;

    f.restart().await;
    let restored = f
        .c()
        .gateway
        .browser_device_management(&f.c().browser)
        .await
        .unwrap();
    assert_eq!(restored[&device].bookmarks[0].id, bookmark.id);
    assert_eq!(restored[&device].bookmarks[0].label, "renamed");
    assert!(
        restored[&device].check.is_none(),
        "restarted host must not reuse live observations"
    );
    f.c()
        .identity
        .revoke_device(&f.c().browser, device)
        .await
        .unwrap();
    assert!(!f
        .c()
        .gateway
        .browser_device_management(&f.c().browser)
        .await
        .unwrap()
        .contains_key(&device));
    assert!(f
        .c()
        .gateway
        .browser_inspect_device(&f.c().browser, device)
        .await
        .is_err());
    assert!(f
        .c()
        .gateway
        .browser_bookmark(
            &f.c().browser,
            device,
            BookmarkCommand::Remove {
                id: bookmark.id,
                revision: renamed[0].revision
            }
        )
        .await
        .is_err());
    f.c()
        .identity
        .delete_device(&f.c().browser, device)
        .await
        .unwrap();
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new().filename(f.root.path().join("cloud.sqlite")),
    )
    .await
    .unwrap();
    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM device_workspace_bookmarks WHERE device_id=?")
            .bind(device.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        remaining, 0,
        "deleting a revoked device clears only its bookmark metadata"
    );
    assert!(f.workspaces[0].path().is_dir());
    pool.close().await;
    assert!(f.script.requests.lock().is_empty());
    assert!(f.submissions.iter().all(|c| c.load(Ordering::SeqCst) == 0));
    f.finish().await;
}

#[tokio::test]
async fn fleet_observes_actual_device_ownership_and_never_recommends_pending_mutations() {
    let f = Fixture::new_for(vec![], true).await;
    let device = f.devices[0].id;
    f.held_submissions[0].store(true, Ordering::SeqCst);
    f.hidden_receipts[0].store(true, Ordering::SeqCst);
    f.script.replies.lock().extend([
        execution_session_tests::call(
            "delegate_task",
            json!({"goal":"write agent.txt"}),
            "fleet-delegate",
        ),
        answer("delegated"),
        tool_write(),
        answer("finished"),
        answer("received"),
    ]);
    f.c()
        .gateway
        .browser_inspect_device(&f.c().browser, device)
        .await
        .unwrap();
    let parent = f
        .c()
        .gateway
        .open_browser_workspace(&f.c().browser, Uuid::new_v4(), device, None)
        .await
        .unwrap();
    let turn = f
        .c()
        .gateway
        .submit_browser_task(&f.c().browser, parent, Uuid::new_v4(), "delegate".into())
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    tokio::time::timeout(Duration::from_secs(20), async {
        while f.submissions[0].load(Ordering::SeqCst) == 0 {
            f.c().gateway.dispatch().await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let state = f
        .c()
        .gateway
        .browser_device_management(&f.c().browser)
        .await
        .unwrap();
    assert!(state[&device].work.mutation_lease.is_some());
    assert_eq!(state[&device].work.unconfirmed_operations, 1);
    assert!(state[&device].recommendation.is_none());
    assert!(state[&device].work.task_states.values().sum::<i64>() > 0);
    assert!(state[&f.devices[1].id].work.mutation_lease.is_none());
    f.c()
        .gateway
        .browser_inspect_device(&f.c().browser, device)
        .await
        .unwrap();
    assert!(f
        .c()
        .gateway
        .browser_device_management(&f.c().browser)
        .await
        .unwrap()[&device]
        .recommendation
        .is_none());
    f.held_submissions[0].store(false, Ordering::SeqCst);
    f.hidden_receipts[0].store(false, Ordering::SeqCst);
    f.submission_release[0].notify_one();
    execution_session_tests::pump(&f, 5).await;
    assert_eq!(f.submissions[0].load(Ordering::SeqCst), 1);
    assert_eq!(f.submissions[1].load(Ordering::SeqCst), 0);
    f.finish().await;
}
