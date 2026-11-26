#[tokio::test]
async fn downstream_cancel_forwards_the_matching_upstream_request_id() {
    let proxy = UpstreamProxy::new("workspace");
    let mock = MockUpstream::new();
    let mut started = mock.held_request_channel();
    let upstream = connect_legacy_upstream(&proxy, mock.clone()).await;
    let downstream = connect_downstream(&proxy, false).await;

    let peer = downstream.client.peer().clone();
    let hold = |slot: &'static str| {
        let peer = peer.clone();
        async move {
            peer.send_cancellable_request(
                ClientRequest::CallToolRequest(rmcp::model::CallToolRequest::new(
                    CallToolRequestParams::new("hold").with_arguments(
                        serde_json::json!({ "slot": slot })
                            .as_object()
                            .cloned()
                            .unwrap_or_default(),
                    ),
                )),
                PeerRequestOptions::no_options(),
            )
            .await
            .expect("hold request starts")
        }
    };
    let first = hold("first").await;
    let second = hold("second").await;
    let mut ids = std::collections::HashMap::new();
    for _ in 0..2 {
        let (id, slot) = tokio::time::timeout(TEST_TIMEOUT, started.recv())
            .await
            .expect("upstream started before deadline")
            .expect("started channel");
        ids.insert(slot, id);
    }
    assert_ne!(ids["first"], ids["second"]);

    first
        .cancel(Some("cancel first".to_owned()))
        .await
        .expect("downstream cancel is sent");
    wait_until(|| mock.cancels().len() == 1).await;
    let cancels = mock.cancels();
    assert_eq!(
        cancels[0].request_id.as_ref(),
        Some(&ids["first"]),
        "取消必须指向被取消调用自己的上游 id"
    );
    assert_ne!(
        cancels[0].request_id.as_ref(),
        Some(&ids["second"]),
        "并发调用之间不得串线"
    );
    assert_eq!(
        cancels[0].reason.as_deref(),
        Some("cancelled by downstream client")
    );

    second
        .cancel(Some("cancel second".to_owned()))
        .await
        .expect("second downstream cancel is sent");
    wait_until(|| mock.cancels().len() == 2).await;
    assert_eq!(mock.cancels()[1].request_id.as_ref(), Some(&ids["second"]));
    assert!(proxy.is_ready(), "取消不是断线，能力保持 ready");

    mock.release_held_call();
    mock.release_held_call();
    stop_server(upstream.server).await;
}

#[tokio::test]
async fn progress_tokens_are_mapped_to_the_downstream_token() {
    let proxy = UpstreamProxy::new("workspace");
    let mock = MockUpstream::new();
    let upstream = connect_legacy_upstream(&proxy, mock.clone()).await;
    let downstream = connect_downstream(&proxy, false).await;

    let handle = downstream
        .client
        .peer()
        .send_cancellable_request(
            ClientRequest::CallToolRequest(rmcp::model::CallToolRequest::new(
                CallToolRequestParams::new("progress"),
            )),
            PeerRequestOptions::no_options(),
        )
        .await
        .expect("progress request starts");
    let downstream_token = handle.progress_token.clone();
    let result = handle.await_response().await.expect("progress call");
    assert!(matches!(
        result,
        rmcp::model::ServerResult::CallToolResult(_)
    ));

    wait_until(|| downstream.recorder.progress().len() == 1).await;
    let progress = downstream.recorder.progress();
    assert_eq!(progress[0].progress_token, downstream_token);
    assert_eq!(progress[0].progress, 1.0);
    assert_eq!(progress[0].message.as_deref(), Some("one"));

    // 未映射的 progress 已在上游发出；给转发留出时间后仍只能有一条。
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        downstream.recorder.progress().len(),
        1,
        "未映射的 progress 必须被丢弃"
    );

    stop_server(upstream.server).await;
}

// ─── task ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn task_ids_are_capability_prefixed_and_mapped_on_every_surface() {
    let proxy = UpstreamProxy::new("workspace");
    let mock = MockUpstream::new();
    let upstream = connect_legacy_upstream(&proxy, mock.clone()).await;
    let downstream = connect_downstream(&proxy, false).await;

    let mut call = CallToolRequestParams::new("task");
    call.meta = Some(owner_meta("session-a"));
    let created = downstream
        .client
        .peer()
        .call_tool_once(call)
        .await
        .expect("task call");
    let task_id = match created {
        CallToolResponse::Task(created) => created.task.task_id,
        _ => panic!("task tool must return CreateTaskResult"),
    };
    assert!(
        task_id.starts_with("workspace:task:"),
        "task id 必须带 capability 前缀: {task_id}"
    );
    assert_ne!(task_id, "up-task-1");

    wait_until(|| downstream.recorder.task_status().contains(&task_id)).await;
    assert_eq!(
        downstream.recorder.task_status(),
        vec![task_id.clone()],
        "早于响应到达的任务通知必须复用同一个下游 id"
    );

    let mut get = GetTaskParams::new(&task_id);
    get.meta = Some(owner_meta("session-a"));
    let result = downstream
        .client
        .peer()
        .get_task(get)
        .await
        .expect("tasks/get");
    assert_eq!(result.task.task.task_id, task_id);
    assert_eq!(result.task.status(), TaskStatus::Completed);

    let mut cancel = CancelTaskParams::new(&task_id);
    cancel.meta = Some(owner_meta("session-a"));
    downstream
        .client
        .peer()
        .cancel_task(cancel)
        .await
        .expect("tasks/cancel");

    let mut update = UpdateTaskParams::new(&task_id, Default::default());
    update.meta = Some(owner_meta("session-a"));
    downstream
        .client
        .peer()
        .update_task(update)
        .await
        .expect("tasks/update");

    let gets = mock.task_gets();
    assert_eq!(gets.len(), 1);
    assert_eq!(gets[0].0, "up-task-1");
    assert_eq!(
        gets[0]
            .1
             .0
             .0
            .get(peri_mcp_core::task_owner::TASK_OWNER_META_KEY)
            .and_then(serde_json::Value::as_str),
        Some("session-a"),
        "task owner 元数据必须原样上行"
    );
    let cancels = mock.task_cancels();
    assert_eq!(cancels.len(), 1);
    assert_eq!(cancels[0].0, "up-task-1");

    let updates = mock.task_updates();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].0, "up-task-1");
    assert_eq!(
        updates[0]
            .1
             .0
             .0
            .get(peri_mcp_core::task_owner::TASK_OWNER_META_KEY)
            .and_then(serde_json::Value::as_str),
        Some("session-a"),
        "tasks/update 的 owner 元数据必须原样上行"
    );

    let call_owner = mock.calls()[0]
        .meta
        .0
         .0
        .get(peri_mcp_core::task_owner::TASK_OWNER_META_KEY)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    assert_eq!(call_owner.as_deref(), Some("session-a"));

    let foreign = GetTaskParams::new("workspace:task:unknown");
    let error = downstream
        .client
        .peer()
        .get_task(foreign)
        .await
        .expect_err("unknown task id must fail");
    assert!(matches!(
        error,
        rmcp::ServiceError::McpError(error) if error.code == ErrorCode::INVALID_PARAMS
    ));
    assert_eq!(mock.task_gets().len(), 1, "未知 task id 不得上行");

    stop_server(upstream.server).await;
}

#[path = "upstream_proxy_wire_test/surfaces.rs"]
mod surfaces;

#[path = "upstream_proxy_wire_test/resilience.rs"]
mod resilience;
