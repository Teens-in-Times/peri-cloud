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
