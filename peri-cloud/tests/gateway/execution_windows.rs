use super::*;

async fn pool(f: &Fixture) -> sqlx::SqlitePool {
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(f.root.path().join("cloud.sqlite"))
                .foreign_keys(true),
        )
        .await
        .unwrap()
}

async fn state(pool: &sqlx::SqlitePool, task: &Value) -> Value {
    let raw: String = sqlx::query_scalar("SELECT record_json FROM agent_tasks WHERE id=?")
        .bind(task.as_str().unwrap())
        .fetch_one(pool)
        .await
        .unwrap();
    serde_json::from_str(&raw).unwrap()
}

async fn until(f: &Fixture, pool: &sqlx::SqlitePool, task: &Value, expected: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            f.c().gateway.dispatch().await.unwrap();
            let current = state(pool, task).await;
            if current["state"] == expected && f.c().runtime.status().active_sessions.is_empty() {
                break current;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("task did not reach {expected}"))
}

async fn submit(f: &Fixture, parent: Uuid, text: &str) {
    let turn = f
        .c()
        .gateway
        .submit_browser_task(&f.c().browser, parent, Uuid::new_v4(), text.into())
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
}

#[tokio::test]
async fn three_proactive_windows_use_tool_free_handoffs_and_continue_without_user_input() {
    let f = Fixture::new_for(
        vec![
            call(
                "delegate_task",
                json!({"goal":"complete across three context boundaries"}),
                "delegate-windows",
            ),
            answer("delegated"),
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
    submit(&f, parent, "work across windows").await;
    let receipt = delegate_receipt(&f.script.requests.lock()[1]);
    let mut work = 0;
    *f.script.dynamic.lock() = Some(Box::new(move |request| {
        let worker = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["content"].as_str())
            .filter_map(|s| serde_json::from_str::<Value>(s).ok())
            .any(|v| v["role"] == "execution_task");
        if worker {
            work += 1;
            if work <= 3 {
                return call(
                    "request_window_rotation",
                    json!({"reason":"next coherent step","notes":format!("finding from window {work}")}),
                    &format!("rotate-{work}"),
                );
            }
            answer("task finished after three rotations")
        } else {
            answer("confirmed worker outcome")
        }
    }));
    let db = pool(&f).await;
    let done = until(&f, &db, &receipt["task_id"], "completed").await;
    assert_eq!(done["execution_session"], receipt["execution_session"]);
    assert_eq!(done["run_number"], 3);
    assert_ne!(done["worker_session"], receipt["worker_session"]);
    {
        let handoffs = f.script.handoff_requests.lock();
        assert_eq!(handoffs.len(), 3);
        for (index, request) in handoffs.iter().enumerate() {
            assert!(request
                .get("tools")
                .is_none_or(|t| t.as_array().is_some_and(Vec::is_empty)));
            assert!(!request.to_string().contains("PRIVATE_REASONING"));
            if index > 0 {
                assert!(
                    request.to_string().contains("Retained finding"),
                    "the next summary must retain the previous attributed handoff"
                );
            }
        }
    }
    let rotations: Vec<String> = sqlx::query_scalar(
        "SELECT record_json FROM agent_window_rotations WHERE task_id=? ORDER BY rowid",
    )
    .bind(receipt["task_id"].as_str().unwrap())
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(rotations.len(), 3);
    assert!(rotations
        .iter()
        .all(|s| serde_json::from_str::<Value>(s).unwrap()["summary_status"] == "complete"));
    {
        let requests = f.script.requests.lock();
        assert!(requests[3]["messages"]
            .to_string()
            .contains("Retained finding"));
        assert_eq!(
            requests[3]["max_tokens"], 8000,
            "actual output must fit the window reserve"
        );
    }
    db.close().await;
    f.finish().await;
}

#[tokio::test]
async fn manual_rotation_without_parent_goal_stays_ready_through_ordinary_chat() {
    let f = Fixture::new_for(
        vec![
            call(
                "delegate_task",
                json!({"goal":"main-owned execution objective"}),
                "delegate-manual",
            ),
            answer("delegated"),
            answer("old window finished current thought"),
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
    let db = pool(&f).await;
    f.script.hold_worker.store(true, Ordering::SeqCst);
    submit(&f, parent, "delegate manual control task").await;
    let receipt = delegate_receipt(&f.script.requests.lock()[1]);
    f.c().gateway.dispatch().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), f.script.worker_waiting.notified())
        .await
        .unwrap();
    let old = state(&db, &receipt["task_id"]).await;
    f.script.replies.lock().extend([call("control_task",json!({"task_id":receipt["task_id"],"expected_revision":old["revision"],"action":"rotate"}),"manual-rotate"),answer("window requested")]);
    submit(&f, parent, "manually change the worker window").await;
    f.script.worker_release.notify_one();
    let current = until(&f, &db, &receipt["task_id"], "window_ready").await;
    assert_ne!(current["worker_session"], receipt["worker_session"]);
    assert_eq!(f.script.handoff_requests.lock().len(), 1);
    let requests = f.script.requests.lock().len();
    for _ in 0..3 {
        f.c().gateway.dispatch().await.unwrap();
    }
    assert_eq!(
        f.script.requests.lock().len(),
        requests,
        "ready is not a new model request"
    );
    f.script
        .replies
        .lock()
        .extend([answer("ordinary conversation continues")]);
    submit(&f, parent, "a separate ordinary question").await;
    assert_eq!(
        state(&db, &receipt["task_id"]).await["state"],
        "window_ready"
    );
    db.close().await;
    f.finish().await;
}

#[tokio::test]
async fn impossible_fresh_context_waits_instead_of_rotating_or_spending_model_requests() {
    let limits = peri_cloud::limits::RuntimeLimits {
        worker_window_tokens: 512,
        worker_reserve_tokens: 64,
        ..Default::default()
    };
    let f = Fixture::new_for_limits(
        vec![
            call(
                "delegate_task",
                json!({"goal":"do not lose requirements"}),
                "delegate-tiny",
            ),
            answer("delegated"),
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
    submit(&f, parent, "delegate").await;
    let receipt = delegate_receipt(&f.script.requests.lock()[1]);
    let db = pool(&f).await;
    let current = until(&f, &db, &receipt["task_id"], "waiting_external").await;
    assert_eq!(current["worker_session"], receipt["worker_session"]);
    assert_eq!(f.script.requests.lock().len(), 2);
    assert!(f.script.handoff_requests.lock().is_empty());
    for _ in 0..3 {
        f.c().gateway.dispatch().await.unwrap();
    }
    assert_eq!(f.script.requests.lock().len(), 2);
    db.close().await;
    f.finish().await;
}

#[tokio::test]
async fn manual_rotation_resumes_a_linked_main_goal_without_giving_worker_a_goal_controller() {
    let f = Fixture::new_for(vec![], true).await;
    let parent = f
        .c()
        .gateway
        .open_browser_workspace(&f.c().browser, Uuid::new_v4(), f.devices[0].id, None)
        .await
        .unwrap();
    let db = pool(&f).await;
    let goal = peri_acp_types::goal::ThreadGoal::new_at(
        "main owns the whole objective".into(),
        None,
        std::time::SystemTime::now(),
    );
    sqlx::query("INSERT INTO cloud_goals VALUES(?,?,?,?)")
        .bind(parent.to_string())
        .bind(f.c().browser.principal().id.to_string())
        .bind(serde_json::to_string(&goal).unwrap())
        .bind(1i64)
        .execute(&db)
        .await
        .unwrap();
    let mode = Arc::new(AtomicU8::new(0));
    let stage = mode.clone();
    let control = Arc::new(Mutex::new(Value::Null));
    let requested = control.clone();
    *f.script.dynamic.lock() = Some(Box::new(move |request| {
        let worker = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["content"].as_str())
            .filter_map(|s| serde_json::from_str::<Value>(s).ok())
            .any(|v| v["role"] == "execution_task");
        if worker {
            return answer("worker completes current work");
        }
        match stage.load(Ordering::SeqCst) {
            0 => {
                stage.store(1, Ordering::SeqCst);
                call(
                    "delegate_task",
                    json!({"goal":"worker sub-objective","link_parent_goal":true}),
                    "delegate-linked",
                )
            }
            2 => {
                stage.store(3, Ordering::SeqCst);
                call("control_task", requested.lock().clone(), "rotate-linked")
            }
            4 => {
                stage.store(5, Ordering::SeqCst);
                call(
                    "ExecuteExtraTool",
                    json!({"tool_name":"goal","params":{"action":"complete"}}),
                    "complete-parent-goal",
                )
            }
            _ => answer("main goal work continues"),
        }
    }));
    let first = f
        .c()
        .gateway
        .submit_browser_task(
            &f.c().browser,
            parent,
            Uuid::new_v4(),
            "delegate part of goal".into(),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if f.script.requests.lock().len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    // Stop this main turn, preserving its active durable goal; workers have separate ownership.
    f.c()
        .gateway
        .cancel_browser_task(&f.c().browser, parent, first.turn_id)
        .await
        .unwrap();
    f.settled(parent, first.turn_id).await;
    let receipt = delegate_receipt(&f.script.requests.lock()[1]);
    assert_eq!(
        state(&db, &receipt["task_id"]).await["parent_goal"],
        goal.goal_id
    );
    f.script.hold_worker.store(true, Ordering::SeqCst);
    f.c().gateway.dispatch().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), f.script.worker_waiting.notified())
        .await
        .unwrap();
    let old = state(&db, &receipt["task_id"]).await;
    *control.lock() =
        json!({"task_id":receipt["task_id"],"expected_revision":old["revision"],"action":"rotate"});
    mode.store(2, Ordering::SeqCst);
    let rotate = f
        .c()
        .gateway
        .submit_browser_task(
            &f.c().browser,
            parent,
            Uuid::new_v4(),
            "manually rotate linked work".into(),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if state(&db, &receipt["task_id"]).await["state"] == "rotate_requested" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    f.c()
        .gateway
        .cancel_browser_task(&f.c().browser, parent, rotate.turn_id)
        .await
        .unwrap();
    f.settled(parent, rotate.turn_id).await;
    mode.store(4, Ordering::SeqCst);
    f.script.worker_release.notify_one();
    let done = until(&f, &db, &receipt["task_id"], "completed").await;
    assert_ne!(done["worker_session"], receipt["worker_session"]);
    assert_eq!(f.script.handoff_requests.lock().len(), 1);
    let goals: Vec<String> =
        sqlx::query_scalar("SELECT session_id FROM cloud_goals WHERE principal=?")
            .bind(f.c().browser.principal().id.to_string())
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(
        goals,
        vec![parent.to_string()],
        "no per-worker goal row exists"
    );
    db.close().await;
    f.finish().await;
}

#[tokio::test]
async fn handoff_races_keep_new_instructions_and_cancellation_authoritative() {
    for cancel in [false, true] {
        let f = Fixture::new_for(
            vec![
                call(
                    "delegate_task",
                    json!({"goal":"preserve newest requirements across handoff"}),
                    "delegate-race",
                ),
                answer("delegated"),
                call(
                    "request_window_rotation",
                    json!({"reason":"handoff race test"}),
                    "rotate-race",
                ),
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
        submit(&f, parent, "delegate").await;
        let receipt = delegate_receipt(&f.script.requests.lock()[1]);
        let db = pool(&f).await;
        f.script.hold_handoff.store(true, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                f.c().gateway.dispatch().await.unwrap();
                if !f.script.handoff_requests.lock().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let old = state(&db, &receipt["task_id"]).await;
        let change = if cancel {
            call(
                "control_task",
                json!({"task_id":receipt["task_id"],"expected_revision":old["revision"],"action":"cancel"}),
                "cancel-in-summary",
            )
        } else {
            call(
                "message_task",
                json!({"task_id":receipt["task_id"],"expected_revision":old["revision"],"body":"NEW_REQUIREMENT_DURING_HANDOFF"}),
                "supplement-in-summary",
            )
        };
        f.script
            .replies
            .lock()
            .extend([change, answer("main remains available")]);
        submit(&f, parent, "change while summary is running").await;
        if cancel {
            f.script
                .replies
                .lock()
                .extend([answer("cancelled result observed")]);
        } else {
            f.script.replies.lock().extend([
                call(
                    "inspect_task",
                    json!({"task_id":receipt["task_id"],"view":"overview"}),
                    "resume-overview",
                ),
                answer("continued with latest requirement"),
                answer("result accepted"),
            ]);
        }
        f.script.handoff_release.notify_one();
        f.script.hold_handoff.store(false, Ordering::SeqCst);
        let current = until(
            &f,
            &db,
            &receipt["task_id"],
            if cancel { "cancelled" } else { "completed" },
        )
        .await;
        assert_eq!(f.script.handoff_requests.lock().len(), 1);
        if cancel {
            assert_eq!(current["worker_session"], receipt["worker_session"]);
        } else {
            assert_ne!(current["worker_session"], receipt["worker_session"]);
            let requests = f.script.requests.lock();
            assert!(requests[5]["messages"]
                .to_string()
                .contains("NEW_REQUIREMENT_DURING_HANDOFF"));
            let overview = tool_value(&requests[6], "handoffs");
            assert_eq!(overview["handoffs"][0]["instruction_watermark"], 0);
            assert!(overview["latest_instruction_watermark"].as_i64().unwrap() > 0);
        }
        db.close().await;
        f.finish().await;
    }
}

#[tokio::test]
async fn unavailable_handoffs_fall_back_once_without_dispatching_tools() {
    for timeout in [false, true] {
        let limits = peri_cloud::limits::RuntimeLimits {
            worker_handoff_seconds: 1,
            ..Default::default()
        };
        let f = Fixture::new_for_limits(
            vec![
                call(
                    "delegate_task",
                    json!({"goal":"continue from host facts if handoff is unavailable"}),
                    "delegate-fallback",
                ),
                answer("delegated"),
                call(
                    "request_window_rotation",
                    json!({"reason":"safe work boundary"}),
                    "rotate-fallback",
                ),
                answer("worker continued using durable facts"),
                answer("outcome received"),
            ],
            true,
            limits,
        )
        .await;
        if timeout {
            f.script.hold_handoff.store(true, Ordering::SeqCst);
        } else {
            // A provider violating tools:[] still has no executor dispatch path.
            f.script.handoff_replies.lock().push_back(tool_write());
        }
        let parent = f
            .c()
            .gateway
            .open_browser_workspace(&f.c().browser, Uuid::new_v4(), f.devices[0].id, None)
            .await
            .unwrap();
        submit(&f, parent, "delegate fallback task").await;
        let receipt = delegate_receipt(&f.script.requests.lock()[1]);
        let db = pool(&f).await;
        let done = until(&f, &db, &receipt["task_id"], "completed").await;
        assert_ne!(done["worker_session"], receipt["worker_session"]);
        assert_eq!(f.script.handoff_requests.lock().len(), 1);
        assert_eq!(f.submissions[0].load(Ordering::SeqCst), 0);
        let raw: String =
            sqlx::query_scalar("SELECT record_json FROM agent_window_rotations WHERE task_id=?")
                .bind(receipt["task_id"].as_str().unwrap())
                .fetch_one(&db)
                .await
                .unwrap();
        let rotation: Value = serde_json::from_str(&raw).unwrap();
        assert!(rotation["summary"].is_null());
        assert_eq!(
            rotation["summary_status"],
            if timeout {
                "unavailable_or_unknown"
            } else {
                "incomplete_or_tool_response"
            }
        );
        f.script.hold_handoff.store(false, Ordering::SeqCst);
        f.script.handoff_release.notify_one();
        db.close().await;
        f.finish().await;
    }
}
