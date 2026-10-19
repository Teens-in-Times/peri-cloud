use super::*;

fn call(name: &str, args: Value, id: &str) -> String {
    let delta = json!({"choices":[{"delta":{"role":"assistant","tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}}]},"finish_reason":"tool_calls"}]});
    format!("data: {delta}\n\ndata: [DONE]\n\n")
}

fn delegate_receipt(request: &Value) -> Value {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| m["content"].as_str())
        .filter_map(|text| serde_json::from_str::<Value>(text).ok())
        .find(|v| v.get("execution_session").is_some())
        .expect("delegation returns a durable task and execution conversation")
}

async fn pump(f: &Fixture, count: usize) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            f.c().gateway.dispatch().await.unwrap();
            if f.script.requests.lock().len() >= count
                && f.c().runtime.status().active_sessions.is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("worker and main result event must settle");
}

fn tool_value(request: &Value, field: &str) -> Value {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| m["content"].as_str())
        .filter_map(|s| serde_json::from_str::<Value>(s).ok())
        .find(|v| v.get(field).is_some())
        .unwrap()
}

#[tokio::test]
async fn recovery_queries_original_receipt_and_never_resubmits_a_missing_or_known_outcome() {
    let mut f = Fixture::new_for(
        vec![
            call(
                "delegate_task",
                json!({"goal":"write exactly once then continue"}),
                "delegate-recover",
            ),
            answer("delegated"),
            tool_write(),
            call(
                "wait_task",
                json!({"reason":"fault injection boundary"}),
                "wait-after-write",
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
    let first = f
        .c()
        .gateway
        .submit_browser_task(&f.c().browser, parent, Uuid::new_v4(), "delegate".into())
        .await
        .unwrap();
    f.settled(parent, first.turn_id).await;
    let receipt = delegate_receipt(&f.script.requests.lock()[1]);
    let window = Uuid::parse_str(receipt["worker_session"].as_str().unwrap()).unwrap();
    let card = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            f.c().gateway.dispatch().await.unwrap();
            if let Some(card) = f
                .c()
                .gateway
                .browser_interactions(&f.c().browser)
                .await
                .unwrap()
                .into_iter()
                .next()
            {
                break card;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    f.c()
        .gateway
        .respond_browser(
            &f.c().browser,
            card.request_id,
            InteractionAction::AllowOnce {},
        )
        .await
        .unwrap();
    pump(&f, 4).await;
    assert_eq!(f.submissions[0].load(Ordering::SeqCst), 1);
    let owner = f.c().browser.principal().id;
    let mut snapshot = f.c().journal.session(owner, window).await.unwrap();
    let write_at = snapshot
        .history
        .iter()
        .position(|p| {
            p.as_message()
                .is_some_and(|m| m.tool_calls().iter().any(|c| c.id == "fixture-write-call"))
        })
        .unwrap();
    snapshot.history.truncate(write_at + 1);
    snapshot.unsettled_jobs = None;
    let raw = serde_json::to_string(&snapshot).unwrap();
    let hash = format!("{:x}", Sha256::digest(raw.as_bytes()));
    let old = f.cloud.take().unwrap();
    old.stop().await;
    drop(old);
    // Model a crash after the executor committed Write but before the cloud
    // persisted its result. The original invocation identity remains intact.
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(f.root.path().join("cloud.sqlite"))
                .foreign_keys(true),
        )
        .await
        .unwrap();
    let task = receipt["task_id"].as_str().unwrap();
    let turn: String =
        sqlx::query_scalar("SELECT json_extract(record_json,'$.turn') FROM agent_tasks WHERE id=?")
            .bind(task)
            .fetch_one(&pool)
            .await
            .unwrap();
    sqlx::query("UPDATE agent_task_checkpoints SET phase='dispatching',snapshot_json=?,snapshot_hash=? WHERE task_id=? AND turn_id=?").bind(&raw).bind(&hash).bind(task).bind(&turn).execute(&pool).await.unwrap();
    sqlx::query(
        "DELETE FROM agent_task_operations WHERE task_id=? AND call_id<>'fixture-write-call'",
    )
    .bind(task)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE agent_task_operations SET receipt_json=NULL,reconciled_json=NULL WHERE task_id=?",
    )
    .bind(task)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE cloud_turns SET state='recovery_required',record_json=json_set(record_json,'$.state','recovery_required') WHERE id=?").bind(&turn).execute(&pool).await.unwrap();
    sqlx::query("UPDATE cloud_sessions SET state_json=? WHERE id=?")
        .bind(&raw)
        .bind(window.to_string())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE agent_tasks SET record_json=json_set(record_json,'$.state','recovery_required','$.notified',json('false')) WHERE id=?").bind(task).execute(&pool).await.unwrap();
    pool.close().await;
    f.hidden_receipts[0].store(true, Ordering::SeqCst);
    f.cloud = Some(
        CloudSide::start_with_limits(
            f.root.path(),
            f.clock.clone(),
            f.connector.clone(),
            f.adapter.clone(),
            false,
            true,
            f.limits.clone(),
        )
        .await,
    );
    for _ in 0..3 {
        f.c().gateway.dispatch().await.unwrap();
    }
    assert_eq!(
        f.script.requests.lock().len(),
        4,
        "missing original receipt must not start a new model run"
    );
    assert_eq!(
        f.submissions[0].load(Ordering::SeqCst),
        1,
        "404 is not permission to resubmit"
    );
    f.hidden_receipts[0].store(false, Ordering::SeqCst);
    f.script.replies.lock().extend([
        answer("confirmed original Write, continued without another effect"),
        answer("recovered result"),
    ]);
    pump(&f, 6).await;
    assert_eq!(
        f.submissions[0].load(Ordering::SeqCst),
        1,
        "known original result must be queried, never replayed"
    );
    assert!(
        f.script.requests.lock()[4]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "tool" && m["tool_call_id"] == "fixture-write-call"),
        "recovered canonical tool result must reach continuation"
    );
    f.finish().await;
}

#[tokio::test]
async fn external_wait_survives_restart_releases_slot_and_resumes_a_new_run() {
    let mut f = Fixture::new_for(
        vec![
            call(
                "delegate_task",
                json!({"goal":"durably wait then finish"}),
                "delegate-wait",
            ),
            answer("delegated"),
            call(
                "wait_task",
                json!({"reason":"external condition"}),
                "external-wait",
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
    let first = f
        .c()
        .gateway
        .submit_browser_task(
            &f.c().browser,
            parent,
            Uuid::new_v4(),
            "wait independently".into(),
        )
        .await
        .unwrap();
    f.settled(parent, first.turn_id).await;
    let receipt = delegate_receipt(&f.script.requests.lock()[1]);
    pump(&f, 3).await;
    assert!(f.c().runtime.status().active_sessions.is_empty());
    f.restart().await;
    f.c().gateway.dispatch().await.unwrap();
    assert_eq!(
        f.script.requests.lock().len(),
        3,
        "restart must not silently replay wait or dispatch work"
    );
    f.script.replies.lock().extend([
        call(
            "inspect_task",
            json!({"task_id":receipt["task_id"]}),
            "inspect-wait",
        ),
        answer("waiting state inspected"),
    ]);
    let inspect = f
        .c()
        .gateway
        .submit_browser_task(
            &f.c().browser,
            parent,
            Uuid::new_v4(),
            "inspect waiting task".into(),
        )
        .await
        .unwrap();
    f.settled(parent, inspect.turn_id).await;
    let mut status = tool_value(&f.script.requests.lock()[4], "state");
    assert_eq!(status["state"], "waiting_external");
    f.script.replies.lock().extend([
        call("control_task",json!({"task_id":receipt["task_id"],"expected_revision":status["revision"],"action":"pause"}),"pause-wait"),
        answer("paused"),
    ]);
    let pause = f
        .c()
        .gateway
        .submit_browser_task(&f.c().browser, parent, Uuid::new_v4(), "pause".into())
        .await
        .unwrap();
    f.settled(parent, pause.turn_id).await;
    f.restart().await;
    f.c().gateway.dispatch().await.unwrap();
    assert_eq!(
        f.script.requests.lock().len(),
        7,
        "paused task must remain paused after restart"
    );
    f.script.replies.lock().extend([
        call(
            "inspect_task",
            json!({"task_id":receipt["task_id"]}),
            "inspect-paused",
        ),
        answer("paused inspected"),
    ]);
    let inspect = f
        .c()
        .gateway
        .submit_browser_task(
            &f.c().browser,
            parent,
            Uuid::new_v4(),
            "inspect paused task".into(),
        )
        .await
        .unwrap();
    f.settled(parent, inspect.turn_id).await;
    status = tool_value(&f.script.requests.lock()[8], "state");
    assert_eq!(status["state"], "paused");
    f.script.replies.lock().extend([call("control_task",json!({"task_id":receipt["task_id"],"expected_revision":status["revision"],"action":"resume"}),"resume-wait"),answer("resuming"),answer("continued task finished"),answer("durable result confirmed")]);
    let resume = f
        .c()
        .gateway
        .submit_browser_task(&f.c().browser, parent, Uuid::new_v4(), "resume".into())
        .await
        .unwrap();
    f.settled(parent, resume.turn_id).await;
    pump(&f, 13).await;
    assert!(f.script.requests.lock()[11]["messages"]
        .to_string()
        .contains("external condition"));
    assert_eq!(f.script.requests.lock().len(), 13);
    f.finish().await;
}

#[tokio::test]
async fn parent_question_wait_is_durable_and_reply_wakes_the_same_task() {
    let mut f = Fixture::new_for(
        vec![
            call(
                "delegate_task",
                json!({"goal":"ask parent then finish"}),
                "delegate-question",
            ),
            answer("delegated"),
            call(
                "message_parent",
                json!({"body":"need parent decision","wait_for_reply":true}),
                "ask-parent",
            ),
            answer("main received the worker question"),
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
    let first = f
        .c()
        .gateway
        .submit_browser_task(&f.c().browser, parent, Uuid::new_v4(), "delegate".into())
        .await
        .unwrap();
    f.settled(parent, first.turn_id).await;
    let receipt = delegate_receipt(&f.script.requests.lock()[1]);
    pump(&f, 4).await;
    f.restart().await;
    f.c().gateway.dispatch().await.unwrap();
    assert_eq!(
        f.script.requests.lock().len(),
        4,
        "delivered question and waiting run must not be replayed"
    );
    f.script.replies.lock().extend([
        call(
            "inspect_task",
            json!({"task_id":receipt["task_id"]}),
            "inspect-question",
        ),
        answer("question still waiting"),
    ]);
    let inspect = f
        .c()
        .gateway
        .submit_browser_task(
            &f.c().browser,
            parent,
            Uuid::new_v4(),
            "inspect question".into(),
        )
        .await
        .unwrap();
    f.settled(parent, inspect.turn_id).await;
    let status = tool_value(&f.script.requests.lock()[5], "state");
    assert_eq!(status["state"], "waiting_parent");
    f.script.replies.lock().extend([
        call("message_task",json!({"task_id":receipt["task_id"],"expected_revision":status["revision"],"body":"parent decision: ready"}),"reply-parent"),answer("reply accepted"),answer("worker used the reply and finished"),answer("parent result confirmed"),
    ]);
    let reply = f
        .c()
        .gateway
        .submit_browser_task(
            &f.c().browser,
            parent,
            Uuid::new_v4(),
            "answer worker".into(),
        )
        .await
        .unwrap();
    f.settled(parent, reply.turn_id).await;
    pump(&f, 10).await;
    assert!(f.script.requests.lock()[8]["messages"]
        .to_string()
        .contains("parent decision: ready"));
    assert_eq!(f.submissions[0].load(Ordering::SeqCst), 0);
    f.finish().await;
}

#[tokio::test]
async fn long_task_continues_after_the_bounded_loop_iteration_limit() {
    let mut responses = vec![
        call(
            "delegate_task",
            json!({"goal":"perform nine small reads"}),
            "delegate-long",
        ),
        answer("delegated"),
    ];
    responses.extend((0..9).map(|i| {
        call(
            "Read",
            json!({"file_path":"small.txt"}),
            &format!("read-step-{i}"),
        )
    }));
    responses.push(answer("all nine reads confirmed"));
    let f = Fixture::new_for(responses, true).await;
    std::fs::write(f.workspaces[0].path().join("small.txt"), "bounded work").unwrap();
    let parent = f
        .c()
        .gateway
        .open_browser_workspace(&f.c().browser, Uuid::new_v4(), f.devices[0].id, None)
        .await
        .unwrap();
    let first = f
        .c()
        .gateway
        .submit_browser_task(&f.c().browser, parent, Uuid::new_v4(), "delegate".into())
        .await
        .unwrap();
    f.settled(parent, first.turn_id).await;
    let receipt = delegate_receipt(&f.script.requests.lock()[1]);
    f.script.replies.lock().extend([
        call(
            "inspect_task",
            json!({"task_id":receipt["task_id"]}),
            "inspect-long",
        ),
        answer("long task result"),
    ]);
    pump(&f, 14).await;
    let status = tool_value(&f.script.requests.lock()[13], "run_number");
    assert_eq!(status["state"], "completed");
    assert_eq!(status["run_number"], 1);
    assert_eq!(status["worker_session"], receipt["worker_session"]);
    assert_eq!(
        f.submissions[0].load(Ordering::SeqCst),
        9,
        "a new run must not replay prior calls"
    );
    f.finish().await;
}

#[tokio::test]
async fn low_budget_rotates_real_windows_retrieves_archived_read_and_hides_them_from_chat() {
    let limits = peri_cloud::limits::RuntimeLimits {
        worker_window_tokens: 512,
        worker_reserve_tokens: 64,
        ..Default::default()
    };
    let f = Fixture::new_for_limits(
        vec![
            call(
                "delegate_task",
                json!({"goal":"read source and continue from archived evidence"}),
                "delegate-rotate",
            ),
            answer("delegated"),
            call(
                "reading",
                json!({"file_path":"source.txt"}),
                "read-original",
            ),
        ],
        true,
        limits,
    )
    .await;
    std::fs::write(
        f.workspaces[0].path().join("source.txt"),
        format!("WINDOW_SOURCE\n{}", "evidence ".repeat(3000)),
    )
    .unwrap();
    let parent = f
        .c()
        .gateway
        .open_browser_workspace(&f.c().browser, Uuid::new_v4(), f.devices[0].id, None)
        .await
        .unwrap();
    let first = f
        .c()
        .gateway
        .submit_browser_task(&f.c().browser, parent, Uuid::new_v4(), "delegate".into())
        .await
        .unwrap();
    f.settled(parent, first.turn_id).await;
    let receipt = delegate_receipt(&f.script.requests.lock()[1]);
    let old_window = Uuid::parse_str(receipt["worker_session"].as_str().unwrap()).unwrap();
    f.script.replies.lock().extend([
        call(
            "inspect_task",
            json!({"task_id":receipt["task_id"],"view":"conversation","window_id":old_window}),
            "retrieve-old",
        ),
        answer("worker finished with original read evidence"),
        call(
            "inspect_task",
            json!({"task_id":receipt["task_id"]}),
            "inspect-rotated",
        ),
        call(
            "inspect_task",
            json!({"task_id":receipt["task_id"],"view":"windows"}),
            "inspect-windows",
        ),
        answer("rotation result confirmed"),
    ]);
    pump(&f, 8).await;
    let (status, windows) = {
        let requests = f.script.requests.lock();
        assert!(
            !requests[3]["messages"]
                .to_string()
                .contains("WINDOW_SOURCE"),
            "new window must not copy the entire old read"
        );
        (
            tool_value(&requests[6], "run_number"),
            tool_value(&requests[7], "windows"),
        )
    };
    assert_eq!(status["task_id"], receipt["task_id"]);
    assert_eq!(status["execution_session"], receipt["execution_session"]);
    assert_ne!(status["worker_session"], receipt["worker_session"]);
    assert!(status["run_number"].as_u64().unwrap() >= 2);
    let retrieved_window =
        Uuid::parse_str(windows["windows"][1]["window_id"].as_str().unwrap()).unwrap();
    let retrieved = f
        .c()
        .journal
        .session(f.c().browser.principal().id, retrieved_window)
        .await
        .unwrap();
    assert!(
        retrieved
            .history
            .iter()
            .filter_map(|p| p.as_message())
            .any(|m| m.content().contains("WINDOW_SOURCE")),
        "archived read retrieval must actually return its evidence"
    );
    assert!(f
        .c()
        .gateway
        .select_logical_conversation(&f.c().browser, Uuid::new_v4(), Some(old_window))
        .await
        .is_err());
    f.finish().await;
}

#[tokio::test]
async fn two_main_conversations_explicitly_reuse_one_executor_context_for_independent_tasks() {
    let f = Fixture::new_for(
        vec![
            call(
                "delegate_task",
                json!({"goal":"first independent task"}),
                "delegate-first",
            ),
            answer("first delegated"),
            answer("worker-first-finished"),
            answer("first result confirmed"),
        ],
        true,
    )
    .await;
    let c = f.c();
    let first_parent = c
        .gateway
        .open_browser_workspace(&c.browser, Uuid::new_v4(), f.devices[0].id, None)
        .await
        .unwrap();
    let first = c
        .gateway
        .submit_browser_task(
            &c.browser,
            first_parent,
            Uuid::new_v4(),
            "delegate first".into(),
        )
        .await
        .unwrap();
    f.settled(first_parent, first.turn_id).await;
    let first_receipt = delegate_receipt(&f.script.requests.lock()[1]);
    pump(&f, 4).await;

    f.script.replies.lock().extend([
        call("delegate_task", json!({"goal":"second independent task","execution_session":first_receipt["execution_session"]}), "delegate-second"),
        answer("second delegated"), tool_write(), answer("worker-second-finished"),
    ]);
    let second_parent = c
        .gateway
        .open_browser_workspace(&c.browser, Uuid::new_v4(), f.devices[0].id, None)
        .await
        .unwrap();
    assert_ne!(first_parent, second_parent);
    let second = c
        .gateway
        .submit_browser_task(
            &c.browser,
            second_parent,
            Uuid::new_v4(),
            "delegate second".into(),
        )
        .await
        .unwrap();
    f.settled(second_parent, second.turn_id).await;
    let second_receipt = delegate_receipt(&f.script.requests.lock()[5]);
    assert_ne!(first_receipt["task_id"], second_receipt["task_id"]);
    assert_eq!(
        first_receipt["execution_session"],
        second_receipt["execution_session"]
    );
    assert_eq!(
        first_receipt["worker_session"],
        second_receipt["worker_session"]
    );
    assert_eq!(second_receipt["note"], "");
    assert_eq!(second_receipt["state"], "queued");
    let card = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            c.gateway.dispatch().await.unwrap();
            if let Some(card) = c
                .gateway
                .browser_interactions(&c.browser)
                .await
                .unwrap()
                .into_iter()
                .next()
            {
                break card;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("worker Write must request its own browser approval");
    assert_eq!(
        card.session_id.to_string(),
        second_receipt["worker_session"].as_str().unwrap()
    );
    f.script.replies.lock().extend([
        call(
            "inspect_task",
            json!({"task_id":second_receipt["task_id"],"view":"operations"}),
            "inspect-receipts",
        ),
        answer("second result confirmed"),
    ]);
    c.gateway
        .respond_browser(&c.browser, card.request_id, InteractionAction::AllowOnce {})
        .await
        .unwrap();
    pump(&f, 10).await;
    let continued = f.script.requests.lock()[6]["messages"].to_string();
    assert!(
        continued.contains("worker-first-finished"),
        "same executor retains prior work context"
    );
    assert!(continued.contains("second independent task"));
    assert!(f.workspaces[0].path().join("agent.txt").exists());
    let operations = f.script.requests.lock()[9]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| m["content"].as_str())
        .filter_map(|s| serde_json::from_str::<Value>(s).ok())
        .find(|v| v.get("entries").is_some())
        .unwrap();
    let write = &operations["entries"][0];
    assert_eq!(write["name"], "Write");
    assert_eq!(write["receipt_recorded"], true);
    assert_eq!(write["execution"]["status"], "completed");
    assert_eq!(write["window_id"], second_receipt["worker_session"]);
    assert_eq!(write["call_hash"].as_str().unwrap().len(), 64);
    c.gateway
        .select_logical_conversation(&c.browser, Uuid::new_v4(), Some(second_parent))
        .await
        .unwrap();
    let chat = c.gateway.web_chat(&c.browser, Some(0)).await.unwrap();
    assert!(chat
        .messages
        .iter()
        .any(|m| m.text.contains("second result confirmed")));
    assert!(!chat
        .messages
        .iter()
        .any(|m| m.text.contains("PRIVATE_REASONING")));
    f.finish().await;
}
