//! Exercises the actual host Connections wrapper, not a direct connector fixture.
use super::*;
async fn command_model(
    State(wire): State<Arc<Wire>>,
    Json(body): Json<Value>,
) -> ([(axum::http::HeaderName, &'static str); 1], String) {
    let summary = body
        .get("tools")
        .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty));
    wire.model_requests.lock().push(body);
    let text = if summary {
        "<summary>HOST_SUMMARY_FACT</summary>"
    } else {
        "HOST_TASK_RESULT"
    };
    let delta =
        json!({"choices":[{"delta":{"role":"assistant","content":text},"finish_reason":"stop"}]});
    (
        [(CONTENT_TYPE, "text/event-stream")],
        format!("data: {delta}\n\ndata: [DONE]\n\n"),
    )
}
async fn idle(process: &Process, client: &Client, account: &Account, session: &str) {
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let rows = process.get(client, account, "/api/conversations").await;
            if rows
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["session_id"] == session && r["phase"] == "idle")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn actual_host_exposes_native_registry_and_executes_context_commands_through_connections() {
    let root = tempfile::tempdir().unwrap();
    let wire = Arc::new(Wire::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let stop = CancellationToken::new();
    let signal = stop.clone();
    let app = Router::new()
        .route("/v1/chat/completions", post(command_model))
        .with_state(wire.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(signal.cancelled_owned())
            .await
            .unwrap();
    });
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    let (path, base) = config(root.path(), &origin, json!([]), Value::Null);
    let mut conf: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    conf["local_instance"] =
        json!({"name":"Native command test","workspace":root.path().join("workspace")});
    std::fs::write(&path, serde_json::to_vec(&conf).unwrap()).unwrap();
    let process = Process::launch(&path, base.clone(), &client).await;
    client.post(format!("{base}/api/setup")).header("Origin",&base).json(&json!({"bootstrap_token":SETUP,"login":"owner","password":PASSWORD,"display_name":"Owner"})).send().await.unwrap().error_for_status().unwrap();
    let account = local_instance_tests::browser_login(&process, &client).await;
    let data = process.get(&client, &account, "/api/account").await;
    let device = data["devices"][0]["id"].clone();
    let catalog = process.get(&client, &account, "/api/commands").await;
    assert!(catalog
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["name"] == "core:compact"));
    let opened = process
        .post(
            &client,
            &account,
            "/api/workspaces/open",
            json!({"request_id":Uuid::new_v4(),"device_id":device}),
        )
        .await;
    let session = opened["session_id"].as_str().unwrap();
    process
        .post(
            &client,
            &account,
            &format!("/api/conversations/{session}/tasks"),
            json!({"request_id":Uuid::new_v4(),"confirmed":true,"prompt":"HOST_OLD_CONTEXT"}),
        )
        .await;
    idle(&process, &client, &account, session).await;
    for command in ["/compact", "/clear"] {
        let response = process
            .post(
                &client,
                &account,
                &format!("/api/conversations/{session}/commands"),
                json!({"request_id":Uuid::new_v4(),"confirmed":true,"command":command}),
            )
            .await;
        assert_eq!(response["state"], "accepted");
        idle(&process, &client, &account, session).await;
        let result = process
            .get(
                &client,
                &account,
                &format!("/api/conversations/{session}/result"),
            )
            .await;
        assert_eq!(result["turn_state"], "completed");
    }
    let created = process
        .post(
            &client,
            &account,
            &format!("/api/conversations/{session}/commands"),
            json!({"request_id":Uuid::new_v4(),"confirmed":true,"command":"/new"}),
        )
        .await;
    assert_ne!(created["session_id"], session);
    assert_eq!(
        wire.model_requests.lock().len(),
        2,
        "clear and new do not call models"
    );
    process.stop(&client, &account).await;
    stop.cancel();
    server.await.unwrap();
}
