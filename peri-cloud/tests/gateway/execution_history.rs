use super::*;

async fn submit(f: &Fixture, parent: Uuid, text: &str) {
    let turn = f
        .c()
        .gateway
        .submit_browser_task(&f.c().browser, parent, Uuid::new_v4(), text.into())
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
}

fn output(request: &Value, call_id: &str) -> String {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool" && m["tool_call_id"] == call_id)
        .unwrap()["content"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn shared_execution_history_is_paginated_read_only_and_requires_explicit_reuse() {
    let f = Fixture::new_for(
        vec![
            call(
                "delegate_task",
                json!({"goal":"PRIVATE_HISTORY_CONTRACT"}),
                "first-task",
            ),
            answer("first delegated"),
            answer("first work completed"),
            answer("first observed"),
        ],
        true,
    )
    .await;
    let first_parent = f
        .c()
        .gateway
        .open_browser_workspace(&f.c().browser, Uuid::new_v4(), f.devices[0].id, None)
        .await
        .unwrap();
    submit(&f, first_parent, "first assignment").await;
    let first = delegate_receipt(&f.script.requests.lock()[1]);
    pump(&f, 4).await;
    let second_parent = f
        .c()
        .gateway
        .open_browser_workspace(&f.c().browser, Uuid::new_v4(), f.devices[0].id, None)
        .await
        .unwrap();
    f.script.replies.lock().extend([
        call(
            "inspect_task",
            json!({"task_id":first["task_id"],"view":"contract"}),
            "before-reuse",
        ),
        answer("history unavailable before sharing"),
    ]);
    submit(&f, second_parent, "inspect before explicit reuse").await;
    let denied = output(&f.script.requests.lock()[5], "before-reuse");
    assert!(denied.contains("outside this conversation"));
    assert!(!denied.contains("PRIVATE_HISTORY_CONTRACT"));

    f.script.replies.lock().extend([
        call(
            "delegate_task",
            json!({"goal":"second assignment","execution_session":first["execution_session"]}),
            "second-task",
        ),
        answer("second delegated"),
        call(
            "inspect_task",
            json!({"task_id":first["task_id"],"view":"contract"}),
            "worker-history",
        ),
        answer("second work completed using relevant prior evidence"),
    ]);
    submit(&f, second_parent, "explicitly reuse execution context").await;
    let second = delegate_receipt(&f.script.requests.lock()[7]);
    f.script.replies.lock().extend([
        call(
            "inspect_task",
            json!({"task_id":second["task_id"],"view":"related_tasks","limit":1}),
            "history-page-one",
        ),
        answer("second observed"),
    ]);
    pump(&f, 12).await;
    assert!(
        output(&f.script.requests.lock()[9], "worker-history").contains("PRIVATE_HISTORY_CONTRACT")
    );
    let page: Value =
        serde_json::from_str(&output(&f.script.requests.lock()[11], "history-page-one")).unwrap();
    assert_eq!(page["tasks"][0]["task_id"], second["task_id"]);
    assert_eq!(page["has_more"], true);
    f.script.replies.lock().extend([
        call("inspect_task", json!({"task_id":second["task_id"],"view":"related_tasks","limit":1,"after":page["next_cursor"]}), "history-page-two"),
        call("message_task", json!({"task_id":first["task_id"],"expected_revision":0,"body":"must not mutate prior task"}), "denied-mutation"),
        call("list_tasks", json!({"limit":1}), "own-tasks"),
        answer("historical evidence read without control rights"),
    ]);
    submit(
        &f,
        second_parent,
        "read next history page and test control isolation",
    )
    .await;
    {
        let requests = f.script.requests.lock();
        let page: Value = serde_json::from_str(&output(&requests[13], "history-page-two")).unwrap();
        assert_eq!(page["tasks"][0]["task_id"], first["task_id"]);
        assert_eq!(page["has_more"], false);
        assert!(output(&requests[14], "denied-mutation").contains("outside this conversation"));
        let own: Value = serde_json::from_str(&output(&requests[15], "own-tasks")).unwrap();
        assert_eq!(own["tasks"].as_array().unwrap().len(), 1);
        assert_eq!(own["tasks"][0]["task_id"], second["task_id"]);
        assert_eq!(own["has_more"], false);
    }
    let third_parent = f
        .c()
        .gateway
        .open_browser_workspace(&f.c().browser, Uuid::new_v4(), f.devices[0].id, None)
        .await
        .unwrap();
    f.script.replies.lock().extend([
        call(
            "inspect_task",
            json!({"task_id":first["task_id"],"view":"contract"}),
            "unrelated-root",
        ),
        answer("unrelated conversation remains isolated"),
    ]);
    submit(&f, third_parent, "try unrelated history").await;
    assert!(!output(&f.script.requests.lock()[17], "unrelated-root")
        .contains("PRIVATE_HISTORY_CONTRACT"));
    assert!(output(&f.script.requests.lock()[17], "unrelated-root")
        .contains("outside this conversation"));
    f.finish().await;
}
