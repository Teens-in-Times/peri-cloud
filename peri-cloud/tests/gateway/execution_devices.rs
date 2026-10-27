use super::*;

#[tokio::test]
async fn per_call_target_is_fixed_approved_exactly_and_unknown_receipts_are_query_only() {
    let mut f = Fixture::new_for(vec![], true).await;
    let device0 = f.devices[0].id;
    let device1 = f.devices[1].id;
    let saved_target = Arc::new(Mutex::new(Value::Null));
    let capture = saved_target.clone();
    let mut step = 0;
    *f.script.dynamic.lock() = Some(Box::new(move |request| {
        let reply = match step {
            0 => call("list_devices", json!({}), "discover"),
            1 => {
                let devices = tool_value(request, "devices");
                assert!(devices["devices"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|d| d["device_id"] == device1.to_string()));
                assert!(!devices.to_string().contains("token"));
                call(
                    "prepare_device",
                    json!({"device_id":device1}),
                    "prepare-other",
                )
            }
            2 => {
                let target = tool_value(request, "target");
                assert_eq!(target["authenticated"], true);
                assert_eq!(target["environment"]["device_id"], device1.to_string());
                assert_eq!(
                    target["environment"]["binding"]["workspace"],
                    target["target"]["workspace"]
                );
                *capture.lock() = target["target"]["target_id"].clone();
                call(
                    "prepare_device",
                    json!({"device_id":device0}),
                    "prepare-original",
                )
            }
            3 => call(
                "Write",
                json!({"device_id":device1,"file_path":"direct.txt","content":"FROZEN_TARGET"}),
                "write-other",
            ),
            4 => call("inspect_device_operations", json!({}), "receipts"),
            5 => {
                let ops = tool_value(request, "operations");
                assert_eq!(ops["operations"].as_array().unwrap().len(), 1);
                let op = &ops["operations"][0];
                assert_eq!(op["device_id"], device1.to_string());
                assert_eq!(op["state"], "confirmed");
                assert!(
                    op["task_id"].is_null(),
                    "small operation creates no delegated task"
                );
                call(
                    "inspect_device_operations",
                    json!({"operation_id":op["operation_id"],"view":"input","chars":16384}),
                    "complete-input",
                )
            }
            6 => {
                let detail = tool_value(request, "version");
                assert_eq!(detail["complete"], true);
                assert!(detail["text"].as_str().unwrap().contains("FROZEN_TARGET"));
                assert!(!detail.to_string().contains("PRIVATE_REASONING"));
                answer("done on explicit target")
            }
            _ => panic!("unexpected step {step}"),
        };
        step += 1;
        reply
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
            "write on the second device".into(),
        )
        .await
        .unwrap();
    let card = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
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
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(card.session_id, parent);
    assert_eq!(
        card.device_id,
        Some(device1),
        "approval must show actual device, not conversation default"
    );
    let peri_acp_types::interaction::InteractionContext::Approval { items } = &card.context else {
        panic!("approval expected")
    };
    assert_eq!(items[0].tool_name, "Write");
    assert_eq!(
        items[0].tool_input["_device_target"]["target_id"],
        *saved_target.lock()
    );
    assert_eq!(f.submissions[1].load(Ordering::SeqCst), 0);
    f.c()
        .gateway
        .respond_browser(
            &f.c().browser,
            card.request_id,
            InteractionAction::AllowOnce {},
        )
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    assert_eq!(
        std::fs::read_to_string(f.workspaces[1].path().join("direct.txt")).unwrap(),
        "FROZEN_TARGET"
    );
    assert!(!f.workspaces[0].path().join("direct.txt").exists());
    assert_eq!(
        f.c()
            .journal
            .session(f.c().browser.principal().id, parent)
            .await
            .unwrap()
            .frozen
            .device_id,
        Some(device0)
    );
    *f.script.dynamic.lock() = None;
    let receipt = tool_value(&f.script.requests.lock()[5], "operations");
    let operation = receipt["operations"][0]["operation_id"]
        .as_str()
        .unwrap()
        .to_string();
    // Simulate loss of the cloud's immediate receipt after the executor committed.
    let old = f.cloud.take().unwrap();
    old.stop().await;
    drop(old);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(f.root.path().join("cloud.sqlite")),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE cloud_native_operations SET state='submitted_unknown',receipt_json=NULL,reconciled_json=NULL WHERE id=?").bind(operation).execute(&pool).await.unwrap();
    pool.close().await;
    f.hidden_receipts[1].store(true, Ordering::SeqCst);
    f.cloud = Some(
        CloudSide::start_with_limits(
            f.root.path(),
            f.clock.clone(),
            f.connector.clone(),
            f.adapter.clone(),
            false,
            f.web,
            f.limits.clone(),
        )
        .await,
    );
    let before = f.script.requests.lock().len();
    f.script.replies.lock().extend([
        call("inspect_device_operations", json!({}), "unknown"),
        answer("original result remains unknown"),
    ]);
    let turn = f
        .c()
        .gateway
        .submit_browser_task(
            &f.c().browser,
            parent,
            Uuid::new_v4(),
            "query original receipt".into(),
        )
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    assert_eq!(
        tool_value(&f.script.requests.lock()[before + 1], "operations")["operations"][0]["state"],
        "submitted_unknown"
    );
    assert_eq!(f.submissions[1].load(Ordering::SeqCst), 1);
    f.hidden_receipts[1].store(false, Ordering::SeqCst);
    let before = f.script.requests.lock().len();
    f.script.replies.lock().extend([
        call("inspect_device_operations", json!({}), "confirmed"),
        answer("original result confirmed"),
    ]);
    let turn = f
        .c()
        .gateway
        .submit_browser_task(
            &f.c().browser,
            parent,
            Uuid::new_v4(),
            "query once available".into(),
        )
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    assert_eq!(
        tool_value(&f.script.requests.lock()[before + 1], "operations")["operations"][0]["state"],
        "confirmed"
    );
    assert_eq!(
        f.submissions[1].load(Ordering::SeqCst),
        1,
        "querying is not resubmission"
    );
    f.script.replies.lock().extend([call("Write",json!({"device_id":device1,"_device_target":{"target_id":*saved_target.lock()},"file_path":"stale.txt","content":"no"}),"stale-target"),answer("previous-turn handle is invalid")]);
    let turn = f
        .c()
        .gateway
        .submit_browser_task(
            &f.c().browser,
            parent,
            Uuid::new_v4(),
            "try expired target".into(),
        )
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    assert_eq!(f.submissions[1].load(Ordering::SeqCst), 1);
    assert!(!f.workspaces[1].path().join("stale.txt").exists());
    f.finish().await;
}

pub(super) async fn next_card(f: &Fixture) -> InteractionCard {
    tokio::time::timeout(Duration::from_secs(15), async {
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
                return card;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn direct_calls_share_worker_mutation_ownership_but_other_device_and_chat_continue() {
    let f = Fixture::new_for(vec![], true).await;
    let device0 = f.devices[0].id;
    let device1 = f.devices[1].id;
    f.script.hold_review.store(true, Ordering::SeqCst);
    let mut step = 0;
    *f.script.dynamic.lock() = Some(Box::new(move |request| {
        let reply = match step {
            0 => call(
                "delegate_task",
                json!({"goal":"write agent.txt on the original device"}),
                "delegate",
            ),
            1 => answer("delegated; we can keep talking"),
            2 => tool_write(),
            3 => call(
                "prepare_device",
                json!({"device_id":device0}),
                "same-device",
            ),
            4 => call(
                "Write",
                json!({"device_id":device0,"file_path":"conflict.txt","content":"must not write"}),
                "conflict",
            ),
            5 => {
                assert!(request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["role"] == "tool"
                        && m["tool_call_id"] == "conflict"
                        && m["content"].as_str().is_some_and(|s| s.contains("changed"))));
                answer("device is reserved by the execution task")
            }
            6 => call(
                "prepare_device",
                json!({"device_id":device1}),
                "other-device",
            ),
            7 => call(
                "Write",
                json!({"device_id":device1,"file_path":"direct.txt","content":"other device remains available"}),
                "independent",
            ),
            8 => answer("small operation completed; still chatting"),
            9 => answer("worker finished original assignment"),
            10 => answer("task result received"),
            _ => panic!("unexpected step {step}"),
        };
        step += 1;
        reply
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
            "delegate original work".into(),
        )
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            f.c().gateway.dispatch().await.unwrap();
            if !f.script.review_requests.lock().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .unwrap();
    let turn = f
        .c()
        .gateway
        .submit_browser_task(
            &f.c().browser,
            parent,
            Uuid::new_v4(),
            "try a small same-device write".into(),
        )
        .await
        .unwrap();
    let card = next_card(&f).await;
    f.c()
        .gateway
        .respond_browser(
            &f.c().browser,
            card.request_id,
            InteractionAction::AllowOnce {},
        )
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    assert!(!f.workspaces[0].path().join("conflict.txt").exists());
    assert_eq!(f.submissions[0].load(Ordering::SeqCst), 0);
    let turn = f
        .c()
        .gateway
        .submit_browser_task(
            &f.c().browser,
            parent,
            Uuid::new_v4(),
            "use the other device while waiting".into(),
        )
        .await
        .unwrap();
    let card = next_card(&f).await;
    assert_eq!(card.device_id, Some(device1));
    f.c()
        .gateway
        .respond_browser(
            &f.c().browser,
            card.request_id,
            InteractionAction::AllowOnce {},
        )
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    assert_eq!(f.submissions[1].load(Ordering::SeqCst), 1);
    assert_eq!(
        f.script.review_requests.lock().len(),
        1,
        "main chat and direct calls do not wait for the worker approval turn"
    );
    f.script.hold_review.store(false, Ordering::SeqCst);
    f.script.review_release.notify_one();
    pump(&f, 11).await;
    assert_eq!(f.submissions[0].load(Ordering::SeqCst), 1);
    assert_eq!(
        std::fs::read_to_string(f.workspaces[0].path().join("agent.txt")).unwrap(),
        "PRIVATE_TOOL_CONTENT"
    );
    assert_eq!(
        std::fs::read_to_string(f.workspaces[1].path().join("direct.txt")).unwrap(),
        "other device remains available"
    );
    f.finish().await;
}

#[tokio::test]
async fn duplicate_native_call_identity_is_rejected_without_reexecuting_the_tool() {
    let f = Fixture::new_for(vec![], true).await;
    f.script.replies.lock().extend([
        call(
            "Read",
            json!({"device_id":f.devices[0].id,"file_path":"source.txt"}),
            "stable-call",
        ),
        call(
            "Read",
            json!({"device_id":f.devices[0].id,"file_path":"source.txt"}),
            "stable-call",
        ),
    ]);
    std::fs::write(
        f.workspaces[0].path().join("source.txt"),
        "original evidence",
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
        .submit_browser_task(
            &f.c().browser,
            parent,
            Uuid::new_v4(),
            "read requested evidence".into(),
        )
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    assert_eq!(
        f.submissions[0].load(Ordering::SeqCst),
        1,
        "a repeated protocol call identity must not POST another executor request"
    );
    {
        let requests = f.script.requests.lock();
        assert!(requests[1]["messages"]
            .to_string()
            .contains("original evidence"));
        assert_eq!(
            requests.len(),
            2,
            "duplicate model tool IDs fail at the protocol boundary"
        );
    }
    f.finish().await;
}
