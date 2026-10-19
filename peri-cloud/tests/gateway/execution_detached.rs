use super::*;

#[tokio::test]
async fn fresh_window_handoff_does_not_survive_a_real_forget_privacy_boundary() {
    use peri_cloud::memory::{MemoryContent, MemoryKind, MemorySource, MemoryStore};
    let limits = peri_cloud::limits::RuntimeLimits {
        worker_window_tokens: 512,
        worker_reserve_tokens: 64,
        ..Default::default()
    };
    let f = Fixture::new_for_limits(
        vec![
            call(
                "delegate_task",
                json!({"goal":"read the source and durably wait"}),
                "delegate",
            ),
            answer("delegated"),
            call("Read", json!({"file_path":"source.txt"}), "read-source"),
        ],
        true,
        limits,
    )
    .await;
    let store = MemoryStore::new(&f.c().journal, f.c().browser.principal().id, 65536);
    let item = store
        .add(
            Uuid::new_v4(),
            0,
            MemoryContent {
                content: "FORGOTTEN_PRIVATE_FIXTURE".into(),
                kind: MemoryKind::Fact,
                pinned: true,
                valid_until: None,
            },
            MemorySource::web(),
        )
        .await
        .unwrap();
    std::fs::write(
        f.workspaces[0].path().join("source.txt"),
        "FORGOTTEN_PRIVATE_FIXTURE",
    )
    .unwrap();
    let parent = f
        .c()
        .gateway
        .open_browser_workspace(&f.c().browser, Uuid::new_v4(), f.devices[0].id, None)
        .await
        .unwrap();
    let turn = f
        .c()
        .gateway
        .submit_browser_task(&f.c().browser, parent, Uuid::new_v4(), "delegate".into())
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    let task = delegate_receipt(&f.script.requests.lock()[1])["task_id"].clone();
    f.script.replies.lock().push_back(call(
        "wait_task",
        json!({"reason":"external condition"}),
        "wait",
    ));
    pump(&f, 4).await;
    let context = f.script.requests.lock()[3]["messages"].to_string();
    assert!(
        context.contains("Most recent bounded tool evidence")
            && context.contains("FORGOTTEN_PRIVATE_FIXTURE")
    );
    store
        .forget(item.id, item.revision, MemorySource::web())
        .await
        .unwrap();
    f.c().gateway.dispatch().await.unwrap();
    let before = f.script.requests.lock().len();
    f.script.replies.lock().extend([
        call(
            "inspect_task",
            json!({"task_id":task}),
            "forbidden-old-task",
        ),
        answer("previous task invalidated by privacy change"),
    ]);
    let turn = f
        .c()
        .gateway
        .submit_browser_task(
            &f.c().browser,
            parent,
            Uuid::new_v4(),
            "inspect old task".into(),
        )
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    {
        let requests = f.script.requests.lock();
        assert_eq!(
            requests.len(),
            before + 2,
            "privacy invalidation does not resume the worker"
        );
        for request in &requests[before..] {
            assert!(!request["messages"]
                .to_string()
                .contains("FORGOTTEN_PRIVATE_FIXTURE"));
        }
        assert!(requests[before + 1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "tool"
                && m["tool_call_id"] == "forbidden-old-task"
                && m["content"]
                    .as_str()
                    .is_some_and(|s| s.contains("outside this conversation"))));
    }
    f.finish().await;
}

#[tokio::test]
async fn running_tool_keeps_original_identity_through_rotation_and_can_settle_or_cancel() {
    for cancel in [false, true] {
        let limits = peri_cloud::limits::RuntimeLimits {
            worker_window_tokens: 512,
            worker_reserve_tokens: 64,
            ..Default::default()
        };
        let mut f = Fixture::new_for_limits(vec![], true, limits).await;
        let facts = Arc::new(Mutex::new(Value::Null));
        let captured = facts.clone();
        let mut step = 0;
        let mut task = Value::Null;
        #[cfg(windows)]
        let command="while (!(Test-Path -LiteralPath 'release.flag')) {Start-Sleep -Milliseconds 50}; [IO.File]::WriteAllText((Join-Path (Get-Location) 'done.txt'),'completed')";
        #[cfg(not(windows))]
        let command =
            "while [ ! -f release.flag ]; do sleep 0.05; done; printf completed > done.txt";
        *f.script.dynamic.lock() = Some(Box::new(move |request| {
            let reply = match step {
                0 => call(
                    "delegate_task",
                    json!({"goal":"start one background operation and continue observing from a new context"}),
                    "delegate",
                ),
                1 => {
                    let receipt = delegate_receipt(request);
                    task = receipt["task_id"].clone();
                    *captured.lock() = receipt;
                    answer("delegated")
                }
                2 => call(
                    "Bash",
                    json!({"command":command,"run_in_background":true}),
                    "original-background",
                ),
                3 => call(
                    "inspect_task",
                    json!({"task_id":task,"view":"operations"}),
                    "observe-original",
                ),
                4 => {
                    let context = request["messages"].to_string();
                    assert!(
                        context.contains("Most recent bounded tool evidence")
                            && context.contains("original-background")
                            && context.contains("running"),
                        "selected operation evidence survives another forced window boundary"
                    );
                    call(
                        "wait_task",
                        json!({"reason":"observe original operation without holding an inference slot"}),
                        "wait-original",
                    )
                }
                _ => panic!("unexpected initial step {step}"),
            };
            step += 1;
            reply
        }));
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
                "delegate background work".into(),
            )
            .await
            .unwrap();
        f.settled(parent, turn.turn_id).await;
        if cancel {
            pump(&f, 5).await;
        } else {
            let task = facts.lock()["task_id"].as_str().unwrap().to_string();
            let mut child = super::crash_tests::launch(&mut f, false).await;
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(
                    sqlx::sqlite::SqliteConnectOptions::new()
                        .filename(f.root.path().join("cloud.sqlite")),
                )
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(20),async {loop {
                assert!(child.try_wait().unwrap().is_none(),"the cloud child remains live until the explicit kill point");
                let settled:i64=sqlx::query_scalar("SELECT COUNT(*) FROM agent_tasks a JOIN cloud_turns t ON t.id=json_extract(a.record_json,'$.turn') WHERE a.id=? AND json_extract(a.record_json,'$.state')='waiting_external' AND t.state NOT IN ('queued','running')").bind(&task).fetch_one(&pool).await.unwrap();
                if settled==1 && f.script.requests.lock().len()==5 {break;}
                tokio::time::sleep(Duration::from_millis(20)).await;
            }}).await.unwrap();
            pool.close().await;
            child.kill().await.unwrap();
            assert!(!child.wait().await.unwrap().success());
            super::crash_tests::reopen(&mut f).await;
        }
        *f.script.dynamic.lock() = None;
        let task = facts.lock()["task_id"].as_str().unwrap().to_string();
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(f.root.path().join("cloud.sqlite")),
            )
            .await
            .unwrap();
        let raw: String = sqlx::query_scalar("SELECT record_json FROM agent_tasks WHERE id=?")
            .bind(&task)
            .fetch_one(&pool)
            .await
            .unwrap();
        let record: Value = serde_json::from_str(&raw).unwrap();
        let invocation: String =
            sqlx::query_scalar("SELECT invocation_id FROM cloud_native_operations WHERE task_id=?")
                .bind(&task)
                .fetch_one(&pool)
                .await
                .unwrap();
        facts.lock()["invocation_id"] = json!(invocation);
        assert_eq!(record["state"], "waiting_external");
        assert_ne!(
            record["worker_session"],
            facts.lock()["worker_session"],
            "new context starts while original tool remains alive"
        );
        assert!(
            f.c().runtime.status().active_sessions.is_empty(),
            "ordinary wait releases inference slot"
        );
        let lease: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM agent_device_leases WHERE device_id=?")
                .bind(f.devices[0].id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(lease, 1, "running original mutation keeps device ownership");
        let original: String = sqlx::query_scalar(
            "SELECT source_session FROM cloud_native_operations WHERE task_id=?",
        )
        .bind(&task)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(original, facts.lock()["worker_session"].as_str().unwrap());
        if !cancel {
            std::fs::write(f.workspaces[0].path().join("release.flag"), "release").unwrap();
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    f.c().gateway.dispatch().await.unwrap();
                    let state: String = sqlx::query_scalar(
                        "SELECT state FROM cloud_native_operations WHERE task_id=?",
                    )
                    .bind(&task)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                    if state == "confirmed" {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            let receipt:String=sqlx::query_scalar("SELECT reconciled_json FROM agent_task_operations WHERE task_id=? AND call_id='original-background'").bind(&task).fetch_one(&pool).await.unwrap();
            let receipt: Value = serde_json::from_str(&receipt).unwrap();
            assert_eq!(receipt["execution"]["status"], "completed");
            assert_eq!(
                std::fs::read_to_string(f.workspaces[0].path().join("done.txt")).unwrap(),
                "completed"
            );
        }
        let revision: i64 = sqlx::query_scalar(
            "SELECT json_extract(record_json,'$.revision') FROM agent_tasks WHERE id=?",
        )
        .bind(&task)
        .fetch_one(&pool)
        .await
        .unwrap();
        let before = f.script.requests.lock().len();
        f.script.replies.lock().extend([call("control_task",json!({"task_id":task,"expected_revision":revision,"action":if cancel{"cancel"}else{"resume"}}),"control"),answer("control accepted")]);
        if !cancel {
            f.script
                .replies
                .lock()
                .push_back(answer("original tool settled; task complete"));
        }
        f.script
            .replies
            .lock()
            .push_back(answer("main received task outcome"));
        let turn = f
            .c()
            .gateway
            .submit_browser_task(
                &f.c().browser,
                parent,
                Uuid::new_v4(),
                if cancel {
                    "cancel the original operation"
                } else {
                    "continue after the original operation settles"
                }
                .into(),
            )
            .await
            .unwrap();
        f.settled(parent, turn.turn_id).await;
        pump(&f, before + if cancel { 3 } else { 4 }).await;
        let raw: String = sqlx::query_scalar("SELECT record_json FROM agent_tasks WHERE id=?")
            .bind(&task)
            .fetch_one(&pool)
            .await
            .unwrap();
        let record: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            record["state"],
            if cancel { "cancelled" } else { "completed" }
        );
        let invocation: String =
            sqlx::query_scalar("SELECT invocation_id FROM cloud_native_operations WHERE task_id=?")
                .bind(&task)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(invocation, facts.lock()["invocation_id"].as_str().unwrap());
        assert_eq!(
            f.submissions[0].load(Ordering::SeqCst),
            1,
            "window changes never resubmit the original tool"
        );
        if cancel {
            let attempts: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM cloud_native_cancellations")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(attempts, 1, "cancel is sent once to the original binding");
            assert!(!f.workspaces[0].path().join("done.txt").exists());
        }
        pool.close().await;
        f.finish().await;
    }
}
