use super::*;

#[tokio::test]
async fn independent_execution_sessions_run_in_parallel_while_reused_session_tasks_serialize() {
    let limits = peri_cloud::limits::RuntimeLimits {
        max_background_turns: 2,
        max_background_per_user: 2,
        ..Default::default()
    };
    let f = Fixture::new_for_limits(vec![], true, limits).await;
    for (index, workspace) in f.workspaces.iter().enumerate() {
        std::fs::write(
            workspace.path().join("source.txt"),
            format!("device {index} source"),
        )
        .unwrap();
    }
    f.held_submissions[0].store(true, Ordering::SeqCst);
    f.held_submissions[1].store(true, Ordering::SeqCst);
    let (device0, device1) = (f.devices[0].id, f.devices[1].id);
    let mut stage = 0;
    let mut first = Value::Null;
    let mut started = std::collections::HashSet::new();
    *f.script.dynamic.lock() = Some(Box::new(move |request| {
        if stage < 4 {
            let reply = match stage {
                0 => call(
                    "delegate_task",
                    json!({"goal":"first task on execution A","device_id":device0}),
                    "first",
                ),
                1 => {
                    first = delegate_receipt(request);
                    call(
                        "delegate_task",
                        json!({"goal":"independent task on execution B","device_id":device1}),
                        "independent",
                    )
                }
                2 => call(
                    "delegate_task",
                    json!({"goal":"second independent assignment on execution A","device_id":device0,"execution_session":first["execution_session"]}),
                    "reuse",
                ),
                _ => answer("three tasks accepted; conversation continues"),
            };
            stage += 1;
            return reply;
        }
        let worker = request["messages"].as_array().unwrap().iter().any(|m| {
            m["role"] == "system"
                && m["content"]
                    .as_str()
                    .is_some_and(|s| s.contains("You are a cloud execution agent"))
        });
        if worker {
            let task = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .rev()
                .filter(|m| m["role"] == "user")
                .filter_map(|m| m["content"].as_str())
                .filter_map(|s| serde_json::from_str::<Value>(s).ok())
                .find(|v| v["role"] == "execution_task")
                .unwrap()["task_id"]
                .as_str()
                .unwrap()
                .to_string();
            if started.insert(task.clone()) {
                call(
                    "Read",
                    json!({"file_path":"source.txt"}),
                    &format!("read-{task}"),
                )
            } else {
                answer("assigned read completed")
            }
        } else {
            answer("task outcome observed")
        }
    }));
    let parent = f
        .c()
        .gateway
        .open_browser_workspace(&f.c().browser, Uuid::new_v4(), device0, None)
        .await
        .unwrap();
    let turn = f
        .c()
        .gateway
        .submit_browser_task(
            &f.c().browser,
            parent,
            Uuid::new_v4(),
            "run independent work and queue a second assignment on A".into(),
        )
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(f.root.path().join("cloud.sqlite")),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            f.c().gateway.dispatch().await.unwrap();
            if f.submissions.iter().all(|s| s.load(Ordering::SeqCst) == 1) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect(
        "both independent execution sessions must reach their executor before either is released",
    );
    let states: Vec<String> = sqlx::query_scalar(
        "SELECT json_extract(record_json,'$.state') FROM agent_tasks ORDER BY rowid",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(states, vec!["running", "running", "queued"]);
    assert_eq!(f.c().runtime.status().active_sessions.len(), 2);
    let records: Vec<String> =
        sqlx::query_scalar("SELECT record_json FROM agent_tasks ORDER BY rowid")
            .fetch_all(&pool)
            .await
            .unwrap();
    let records: Vec<Value> = records
        .iter()
        .map(|r| serde_json::from_str(r).unwrap())
        .collect();
    assert_eq!(
        records[0]["execution_session"],
        records[2]["execution_session"]
    );
    assert_eq!(records[0]["worker_session"], records[2]["worker_session"]);
    assert_ne!(
        records[0]["execution_session"],
        records[1]["execution_session"]
    );
    assert_ne!(records[0]["id"], records[2]["id"]);
    f.held_submissions[0].store(false, Ordering::SeqCst);
    f.submission_release[0].notify_one();
    tokio::time::timeout(Duration::from_secs(20),async {loop {
        f.c().gateway.dispatch().await.unwrap();
        let completed:i64=sqlx::query_scalar("SELECT COUNT(*) FROM agent_tasks WHERE json_extract(record_json,'$.state')='completed'").fetch_one(&pool).await.unwrap();
        if completed==2 {break;}
        tokio::time::sleep(Duration::from_millis(20)).await;
    }}).await.unwrap();
    assert_eq!(
        f.submissions[0].load(Ordering::SeqCst),
        2,
        "A accepted its second task after its first task settled"
    );
    assert_eq!(
        f.submissions[1].load(Ordering::SeqCst),
        1,
        "B remained independently active"
    );
    f.held_submissions[1].store(false, Ordering::SeqCst);
    f.submission_release[1].notify_one();
    pump(&f, 13).await;
    let completed: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_tasks WHERE json_extract(record_json,'$.state')='completed'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(completed, 3);
    assert!(
        f.script.review_requests.lock().is_empty(),
        "read-only work did not create spurious approval or deletion notices"
    );
    pool.close().await;
    f.finish().await;
}
