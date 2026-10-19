use super::*;

#[tokio::test]
async fn artifacts_use_real_receipts_and_latest_requirements_invalidate_acceptance() {
    let mut f = Fixture::new_for(vec![], true).await;
    let mut step = 0;
    let mut task = Value::Null;
    let mut write = Value::Null;
    let mut artifact = Value::Null;
    let mut revision = Value::Null;
    let mut ready = Value::Null;
    *f.script.dynamic.lock() = Some(Box::new(move |request| {
        let reply = match step {
            0 => call(
                "delegate_task",
                json!({"goal":"write agent.txt and verify its content","acceptance":"file contains PRIVATE_TOOL_CONTENT"}),
                "delegate",
            ),
            1 => {
                task = delegate_receipt(request)["task_id"].clone();
                answer("delegated")
            }
            2 => tool_write(),
            3 => call(
                "inspect_task",
                json!({"task_id":task,"view":"operations"}),
                "ops-write",
            ),
            4 => {
                let ops = tool_value(request, "entries");
                write = ops["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|o| o["name"] == "Write")
                    .unwrap()["cursor"]
                    .clone();
                call("inspect_task", json!({"task_id":task}), "status")
            }
            5 => {
                let status = tool_value(request, "revision");
                call(
                    "record_artifact",
                    json!({"task_id":task,"expected_revision":status["revision"],"operation":write,"label":"requested file"}),
                    "artifact",
                )
            }
            6 => {
                let delivery = tool_value(request, "delivery");
                artifact = delivery["delivery"]["artifacts"][0]["id"].clone();
                revision = delivery["revision"].clone();
                call("Read", json!({"file_path":"agent.txt"}), "verify-read")
            }
            7 => call(
                "inspect_task",
                json!({"task_id":task,"view":"operations"}),
                "ops-read",
            ),
            8 => {
                let ops = tool_value(request, "entries");
                let read = ops["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|o| o["name"] == "Read")
                    .unwrap()["cursor"]
                    .clone();
                call(
                    "record_verification",
                    json!({"task_id":task,"expected_revision":revision,"operation":read,"artifacts":[artifact],"label":"Read actual file and compare requested text"}),
                    "verified",
                )
            }
            9 => answer("file produced and read back"),
            10 => call("inspect_deliverables", json!({"task_id":task}), "evidence"),
            11 => {
                let delivery = tool_value(request, "delivery");
                assert_eq!(
                    delivery["accepted"], false,
                    "loop completion is not acceptance"
                );
                assert_eq!(delivery["evidence_current"], true);
                let artifact = &delivery["delivery"]["artifacts"][0];
                assert_eq!(artifact["path"], "agent.txt");
                assert!(artifact["content_version"]
                    .as_str()
                    .unwrap()
                    .starts_with("sha256:"));
                ready = delivery;
                call(
                    "review_task",
                    json!({"task_id":task,"expected_revision":ready["revision"],"outcome":"accepted","reason":"claim without evidence"}),
                    "unsupported-accept",
                )
            }
            12 => {
                assert!(request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["role"] == "tool"
                        && m["tool_call_id"] == "unsupported-accept"
                        && m["content"].as_str().is_some_and(|s| s.contains("changed"))));
                call(
                    "review_task",
                    json!({"task_id":task,"expected_revision":ready["revision"],"outcome":"accepted","reason":"Actual Write receipt and Read output match the required content","verifications":[ready["delivery"]["verifications"][0]["id"]]}),
                    "accept",
                )
            }
            13 => {
                assert_eq!(tool_value(request, "delivery")["accepted"], true);
                answer("verified and accepted")
            }
            _ => panic!("unexpected model step {step}"),
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
            "make the file".into(),
        )
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    pump(&f, 14).await;
    let task = delegate_receipt(&f.script.requests.lock()[1])["task_id"].clone();
    assert_eq!(
        std::fs::read_to_string(f.workspaces[0].path().join("agent.txt")).unwrap(),
        "PRIVATE_TOOL_CONTENT"
    );
    *f.script.dynamic.lock() = None;
    let before = f.script.requests.lock().len();
    // A completed execution remains open to new requirements; it is not silently rerun.
    f.script.replies.lock().extend([call(
        "inspect_task",
        json!({"task_id":task}),
        "status-after",
    )]);
    // Choose the current revision from durable storage, not a guessed model value.
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(f.root.path().join("cloud.sqlite")),
        )
        .await
        .unwrap();
    let revision: i64 = sqlx::query_scalar(
        "SELECT json_extract(record_json,'$.revision') FROM agent_tasks WHERE id=?",
    )
    .bind(task.as_str().unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    f.script.replies.lock().extend([
        call("message_task",json!({"task_id":task,"expected_revision":revision,"body":"Also require an additional footer before acceptance"}),"new-requirement"),
        call("inspect_deliverables",json!({"task_id":task}),"stale-acceptance"),
        answer("previous acceptance is stale; continuation required"),
    ]);
    let turn = f
        .c()
        .gateway
        .submit_browser_task(
            &f.c().browser,
            parent,
            Uuid::new_v4(),
            "add a requirement".into(),
        )
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    assert_eq!(
        tool_value(&f.script.requests.lock()[before + 3], "delivery")["accepted"],
        false
    );
    assert_eq!(
        f.submissions[0].load(Ordering::SeqCst),
        2,
        "Write and Read only; new requirement must not silently replay effects"
    );
    f.restart().await;
    let before = f.script.requests.lock().len();
    f.script.replies.lock().extend([
        call(
            "inspect_deliverables",
            json!({"task_id":task}),
            "restored-evidence",
        ),
        answer("evidence retained"),
    ]);
    let turn = f
        .c()
        .gateway
        .submit_browser_task(
            &f.c().browser,
            parent,
            Uuid::new_v4(),
            "inspect after restart".into(),
        )
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    let delivery = tool_value(&f.script.requests.lock()[before + 1], "delivery");
    assert_eq!(delivery["accepted"], false);
    assert_eq!(delivery["delivery"]["reviews"][0]["outcome"], "accepted");
    assert!(!delivery.to_string().contains("PRIVATE_REASONING"));
}
