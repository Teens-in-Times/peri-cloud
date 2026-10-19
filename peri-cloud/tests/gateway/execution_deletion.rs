use super::*;

#[tokio::test]
async fn deletion_notice_uses_actual_shell_semantics_and_precedes_nested_file_removal() {
    let f = Fixture::new_for(
        vec![
            call(
                "delegate_task",
                json!({"goal":"only print hello"}),
                "safe-task",
            ),
            answer("delegated"),
            call("Bash", json!({"command":"echo hello"}), "literal-output"),
            answer("printed"),
            answer("result"),
        ],
        true,
    )
    .await;
    let parent = f
        .c()
        .gateway
        .open_browser_workspace(&f.c().browser, Uuid::new_v4(), f.devices[0].id, None)
        .await
        .unwrap();
    let turn = f
        .c()
        .gateway
        .submit_browser_task(&f.c().browser, parent, Uuid::new_v4(), "print hello".into())
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    pump(&f, 5).await;
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(f.root.path().join("cloud.sqlite")),
        )
        .await
        .unwrap();
    let notices: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM gateway_outbox WHERE semantic_key LIKE 'task-deletion:%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        notices, 0,
        "a literal echo is not deletion merely because the tool is Bash"
    );
    std::fs::write(
        f.workspaces[0].path().join("discard.txt"),
        "explicitly disposable fixture",
    )
    .unwrap();
    #[cfg(windows)]
    let command = "[IO.File]::Delete((Join-Path (Get-Location) 'discard.txt'))";
    #[cfg(not(windows))]
    let command = "sh -c 'rm -- discard.txt'";
    f.script.hold_review.store(true, Ordering::SeqCst);
    f.script.review_replies.lock().push_back(answer(&json!({"approve":true,"deletion_possible":true,"reason":"exact nested file deletion requested by user","notice":"我将删除任务指定的临时文件，然后继续。"}).to_string()));
    f.script.replies.lock().extend([
        call(
            "delegate_task",
            json!({"goal":"delete exactly the disposable discard.txt file"}),
            "delete-task",
        ),
        answer("deletion delegated"),
        call("Bash", json!({"command":command}), "nested-delete"),
        answer("deleted"),
        answer("deletion result"),
    ]);
    let turn = f
        .c()
        .gateway
        .submit_browser_task(
            &f.c().browser,
            parent,
            Uuid::new_v4(),
            "remove the disposable file".into(),
        )
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            f.c().gateway.dispatch().await.unwrap();
            if f.script.review_requests.lock().len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(f.script.review_requests.lock()[1]["messages"]
        .to_string()
        .contains("discard.txt"));
    assert!(f.workspaces[0].path().join("discard.txt").exists());
    f.script.hold_review.store(false, Ordering::SeqCst);
    f.script.review_release.notify_one();
    // No gateway dispatcher runs while the notice is only queued.
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let notices: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM gateway_outbox WHERE semantic_key LIKE 'task-deletion:%'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if notices == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        f.workspaces[0].path().join("discard.txt").exists(),
        "queued notice alone cannot authorize the actual deletion"
    );
    assert_eq!(f.submissions[0].load(Ordering::SeqCst), 1);
    pump(&f, 10).await;
    assert!(!f.workspaces[0].path().join("discard.txt").exists());
    assert_eq!(f.submissions[0].load(Ordering::SeqCst), 2);
    let delivered:i64=sqlx::query_scalar("SELECT COUNT(*) FROM gateway_outbox WHERE semantic_key LIKE 'task-deletion:%' AND state='delivered'").fetch_one(&pool).await.unwrap();
    assert_eq!(delivered, 1);
    assert!(
        f.c()
            .gateway
            .browser_interactions(&f.c().browser)
            .await
            .unwrap()
            .is_empty(),
        "worker deletion required no human approval"
    );
    pool.close().await;
    f.finish().await;
}
