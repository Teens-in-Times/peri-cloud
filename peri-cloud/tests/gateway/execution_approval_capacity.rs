use super::*;

#[tokio::test]
async fn approval_wait_releases_capacity_for_another_execution_session_of_the_same_user() {
    let f = Fixture::new_for(vec![], true).await;
    std::fs::write(
        f.workspaces[1].path().join("source.txt"),
        "capacity evidence",
    )
    .unwrap();
    f.script.hold_review.store(true, Ordering::SeqCst);
    f.held_submissions[1].store(true, Ordering::SeqCst);
    f.script.replies.lock().extend([
        call("delegate_task",json!({"goal":"write agent.txt on device zero"}),"delegate-review"),answer("first delegated"),tool_write(),
        call("delegate_task",json!({"goal":"read source.txt on another device while the first review waits","device_id":f.devices[1].id}),"delegate-other"),answer("second delegated; chatting continues"),
        call("Read",json!({"file_path":"source.txt"}),"independent-read"),answer("independent task finished"),answer("independent result received"),
        answer("first write finished"),answer("first result received"),
    ]);
    let parent = f
        .c()
        .gateway
        .open_browser_workspace(&f.c().browser, Uuid::new_v4(), f.devices[0].id, None)
        .await
        .unwrap();
    let turn = f
        .c()
        .gateway
        .submit_browser_task(
            &f.c().browser,
            parent,
            Uuid::new_v4(),
            "delegate first".into(),
        )
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            f.c().gateway.dispatch().await.unwrap();
            if f.script.review_requests.lock().len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(f.c().runtime.limits().max_background_per_user, 1);
    let turn = f
        .c()
        .gateway
        .submit_browser_task(
            &f.c().browser,
            parent,
            Uuid::new_v4(),
            "delegate independent work".into(),
        )
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            f.c().gateway.dispatch().await.unwrap();
            if f.script.requests.lock().len() >= 6
                && f.submissions[1].load(Ordering::SeqCst) == 1
                && !f.c().runtime.status().active_sessions.contains(&parent)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a waiting approval must not hold the only per-user background slot");
    assert_eq!(
        f.submissions[0].load(Ordering::SeqCst),
        0,
        "first task is still awaiting independent review"
    );
    assert_eq!(
        f.submissions[1].load(Ordering::SeqCst),
        1,
        "another logical execution session used the released slot"
    );
    assert_eq!(f.script.review_requests.lock().len(), 1);
    f.script.hold_review.store(false, Ordering::SeqCst);
    f.script.review_release.notify_one();
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(f.root.path().join("cloud.sqlite")),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15),async {loop {
        f.c().gateway.dispatch().await.unwrap();
        let waiting:i64=sqlx::query_scalar("SELECT COUNT(*) FROM agent_tasks WHERE json_extract(record_json,'$.state')='waiting_capacity'").fetch_one(&pool).await.unwrap();
        if waiting==1 {break;}
        tokio::time::sleep(Duration::from_millis(20)).await;
    }}).await.expect("reviewed task waits for the independently occupied execution slot");
    assert_eq!(
        f.submissions[0].load(Ordering::SeqCst),
        0,
        "approval cannot bypass the per-user execution capacity"
    );
    pool.close().await;
    f.held_submissions[1].store(false, Ordering::SeqCst);
    f.submission_release[1].notify_one();
    pump(&f, 10).await;
    assert_eq!(
        f.submissions[0].load(Ordering::SeqCst),
        1,
        "reviewed task reacquires compute capacity before execution"
    );
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(f.root.path().join("cloud.sqlite")),
        )
        .await
        .unwrap();
    let complete: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_tasks WHERE json_extract(record_json,'$.state')='completed'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(complete, 2);
    pool.close().await;
    f.finish().await;
}
