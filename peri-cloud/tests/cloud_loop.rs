use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::routing::post;
use axum::{Json, Router};
use parking_lot::Mutex;
use peri_acp_types::event_v2::RenderEvent;
use peri_acp_types::interaction::{
    ApprovalDecision, InteractionContext, InteractionResponse, UserInteractionBroker,
};
use peri_acp_types::messages::{BaseMessage, MessageContent};
use peri_acp_types::permission::{PermissionMode, SharedPermissionMode};
use peri_acp_types::store::PersistedPayload;
use peri_cloud::state::{CloudJournal, TurnState};
use peri_cloud::{CloudAgent, CloudTurnRequest, TurnStatus};
use peri_cloud::{CloudRuntime, SubmitTurn};
use peri_executor::{web, Executor};
use peri_model::{OpenAiConfig, OpenAiModel};
use peri_remote_tools::{DeviceClient, RemoteSession};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Default)]
struct Script {
    replies: Mutex<VecDeque<String>>,
    requests: Mutex<Vec<Value>>,
}

async fn model_request(
    State(script): State<Arc<Script>>,
    Json(request): Json<Value>,
) -> ([(axum::http::HeaderName, &'static str); 1], String) {
    script.requests.lock().push(request);
    let response = script
        .replies
        .lock()
        .pop_front()
        .expect("unexpected model request");
    ([(CONTENT_TYPE, "text/event-stream")], response)
}

fn tool_call(id: &str, name: &str, input: Value) -> String {
    let delta = json!({"choices":[{"delta":{"role":"assistant","tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}}]},"finish_reason":"tool_calls"}]});
    format!("data: {delta}\n\ndata: [DONE]\n\n")
}

fn answer(text: &str) -> String {
    let delta =
        json!({"choices":[{"delta":{"role":"assistant","content":text},"finish_reason":"stop"}]});
    format!("data: {delta}\n\ndata: [DONE]\n\n")
}

struct Broker {
    approve: bool,
    requests: Mutex<Vec<InteractionContext>>,
}

#[async_trait]
impl UserInteractionBroker for Broker {
    async fn request(&self, context: InteractionContext) -> InteractionResponse {
        self.requests.lock().push(context.clone());
        match context {
            InteractionContext::Approval { items } => InteractionResponse::Decisions(
                items
                    .into_iter()
                    .map(|_| {
                        if self.approve {
                            ApprovalDecision::Approve {
                                source: Some("test-channel".into()),
                            }
                        } else {
                            ApprovalDecision::Reject {
                                reason: "用户拒绝本次执行".into(),
                                source: Some("test-channel".into()),
                            }
                        }
                    })
                    .collect(),
            ),
            InteractionContext::Questions { .. } => InteractionResponse::Rejected,
        }
    }
}

struct Harness {
    _state: tempfile::TempDir,
    workspace: tempfile::TempDir,
    executor: Arc<Executor>,
    agent: Arc<CloudAgent>,
    script: Arc<Script>,
    broker: Arc<Broker>,
    transport_token: String,
    stop: CancellationToken,
    servers: Vec<tokio::task::JoinHandle<()>>,
}

impl Harness {
    async fn new(responses: Vec<String>, approve: bool) -> Self {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("peri_agent=debug,peri_cloud=debug,peri_model=debug")
            .with_test_writer()
            .try_init();
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let executor = Arc::new(
            Executor::open(state.path(), "cloud-loop-test")
                .await
                .unwrap(),
        );
        let transport_token =
            std::fs::read_to_string(state.path().join("transport-token")).unwrap();
        let stop = CancellationToken::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = web::router(executor.clone());
        let shutdown = stop.clone();
        let executor_server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .unwrap();
        });
        let client =
            DeviceClient::connect(address, transport_token.clone(), executor.info().device_id)
                .await
                .unwrap();
        let cloud_session = Uuid::new_v4();
        let remote = RemoteSession::open(
            client,
            Uuid::new_v4(),
            cloud_session.to_string(),
            workspace.path().to_str().unwrap(),
        )
        .await
        .unwrap();
        let script = Arc::new(Script {
            replies: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        });
        let model_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let model_address = model_listener.local_addr().unwrap();
        let model_app = Router::new()
            .route("/v1/chat/completions", post(model_request))
            .with_state(script.clone());
        let shutdown = stop.clone();
        let model_server = tokio::spawn(async move {
            axum::serve(model_listener, model_app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .unwrap();
        });
        let model = Arc::new(OpenAiModel::new(OpenAiConfig::new(
            format!("http://{model_address}/v1").parse().unwrap(),
            "test-model-key",
            "fixture-model",
        )));
        let agent = Arc::new(CloudAgent::new(cloud_session, remote, model, "You are a personal cloud agent. Native tools operate only on the bound device. Keep tool events internal.".into(), 128_000).unwrap());
        let broker = Arc::new(Broker {
            approve,
            requests: Mutex::new(Vec::new()),
        });
        Self {
            _state: state,
            workspace,
            executor,
            agent,
            script,
            broker,
            transport_token,
            stop,
            servers: vec![executor_server, model_server],
        }
    }

    async fn run(
        &self,
        mode: PermissionMode,
        history: Vec<PersistedPayload>,
    ) -> peri_cloud::CloudTurnResult {
        tokio::time::timeout(
            Duration::from_secs(20),
            self.agent.run_turn(CloudTurnRequest {
                turn_id: Uuid::new_v4(),
                prompt: MessageContent::Text("完成这次任务".into()),
                history,
                history_flags: HashMap::new(),
                broker: self.broker.clone(),
                permissions: SharedPermissionMode::new(mode),
                classifier: None,
                cancellation: CancellationToken::new(),
                max_iterations: 8,
            }),
        )
        .await
        .unwrap_or_else(|_| {
            panic!(
                "loop did not settle: model_requests={}, pending_responses={}, approval_requests={}",
                self.script.requests.lock().len(),
                self.script.replies.lock().len(),
                self.broker.requests.lock().len(),
            )
        })
        .unwrap()
    }

    async fn close(self) {
        self.stop.cancel();
        for server in self.servers {
            server.await.unwrap();
        }
        self.executor.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn original_loop_runs_remote_write_and_read_through_cloud_permission_and_returns_only_assistant_reply(
) {
    let h = Harness::new(
        vec![
            tool_call(
                "write-call",
                "Write",
                json!({"file_path":"note.txt","content":"device-result"}),
            ),
            tool_call("read-call", "Read", json!({"file_path":"note.txt"})),
            answer("文件已经完成。"),
        ],
        true,
    )
    .await;
    let result = h.run(PermissionMode::Default, Vec::new()).await;
    assert_eq!(result.status, TurnStatus::Completed);
    assert_eq!(
        result
            .replies
            .iter()
            .map(|reply| reply.text.as_str())
            .collect::<Vec<_>>(),
        ["文件已经完成。"]
    );
    assert_eq!(
        std::fs::read_to_string(h.workspace.path().join("note.txt")).unwrap(),
        "device-result"
    );
    assert_eq!(h.broker.requests.lock().len(), 1);
    let approvals = h.broker.requests.lock().clone();
    let InteractionContext::Approval { items } = &approvals[0] else {
        panic!("expected cloud approval");
    };
    assert_eq!(items[0].tool_name, "Write");
    assert_eq!(items[0].tool_input["file_path"], "note.txt");
    assert!(result
        .history
        .iter()
        .any(|payload| matches!(payload, PersistedPayload::Message(BaseMessage::Tool { .. }))));
    assert!(result
        .internal_events
        .iter()
        .any(|event| matches!(event, RenderEvent::ToolStarted { .. })));
    assert!(result
        .internal_events
        .iter()
        .any(|event| matches!(event, RenderEvent::ToolEnded { .. })));
    assert!(result.unsettled_jobs.unwrap().is_empty());
    let requests = h.script.requests.lock().clone();
    assert_eq!(requests.len(), 3);
    for request in requests.iter() {
        assert!(!request.to_string().contains(&h.transport_token));
        for tool in request["tools"].as_array().unwrap() {
            assert!(!tool["function"]["parameters"].to_string().contains("ssh"));
        }
    }
    h.close().await;
}

#[tokio::test]
async fn cloud_rejection_prevents_device_execution_and_remains_in_canonical_history() {
    let h = Harness::new(
        vec![
            tool_call(
                "write-call",
                "Write",
                json!({"file_path":"rejected.txt","content":"must-not-write"}),
            ),
            answer("已取消本次修改。"),
        ],
        false,
    )
    .await;
    let result = h.run(PermissionMode::Default, Vec::new()).await;
    assert_eq!(result.status, TurnStatus::Completed);
    assert!(!h.workspace.path().join("rejected.txt").exists());
    assert!(result.history.iter().any(|payload| matches!(
        payload,
        PersistedPayload::Message(BaseMessage::Tool { is_error: true, .. })
    )));
    assert_eq!(h.broker.requests.lock().len(), 1);
    assert_eq!(result.replies[0].text, "已取消本次修改。");
    assert!(result.unsettled_jobs.unwrap().is_empty());
    h.close().await;
}

#[tokio::test]
async fn shell_alias_uses_canonical_bash_approval_and_returns_real_exit_evidence() {
    #[cfg(windows)]
    let command = "[Console]::Out.Write('shell-result')";
    #[cfg(unix)]
    let command = "printf shell-result";
    let h = Harness::new(
        vec![
            tool_call("shell-call", "Shell", json!({"command":command})),
            answer("命令执行完成。"),
        ],
        true,
    )
    .await;
    let result = h.run(PermissionMode::Default, Vec::new()).await;
    assert_eq!(result.status, TurnStatus::Completed);
    let approvals = h.broker.requests.lock().clone();
    let InteractionContext::Approval { items } = &approvals[0] else {
        panic!("expected approval");
    };
    assert_eq!(items[0].tool_name, "Bash");
    assert!(result.history.iter().any(|payload| matches!(payload, PersistedPayload::Message(BaseMessage::Tool { execution:Some(evidence), .. }) if evidence.exit_code == Some(0))));
    assert_eq!(result.replies[0].text, "命令执行完成。");
    h.close().await;
}

#[tokio::test]
async fn accept_edit_reuses_existing_policy_and_next_turn_receives_prior_canonical_history() {
    let h = Harness::new(
        vec![
            tool_call(
                "edit-mode-call",
                "Write",
                json!({"file_path":"accepted.txt","content":"accepted"}),
            ),
            answer("已写入。"),
            answer("我保留了上次的会话。"),
        ],
        false,
    )
    .await;
    let first = h.run(PermissionMode::AcceptEdit, Vec::new()).await;
    assert_eq!(first.status, TurnStatus::Completed);
    assert_eq!(
        std::fs::read_to_string(h.workspace.path().join("accepted.txt")).unwrap(),
        "accepted"
    );
    assert!(h.broker.requests.lock().is_empty());
    let second = h.run(PermissionMode::Default, first.history).await;
    assert_eq!(second.replies.len(), 1);
    assert_eq!(second.replies[0].text, "我保留了上次的会话。");
    assert!(h.script.requests.lock().last().unwrap()["messages"]
        .to_string()
        .contains("已写入。"));
    h.close().await;
}

#[tokio::test]
async fn deployment_owned_turn_survives_aborted_admission_waiter_and_duplicate_gateway_message() {
    let h = Harness::new(
        vec![
            tool_call(
                "durable-write",
                "Write",
                json!({"file_path":"durable.txt","content":"only-once","append":true}),
            ),
            answer("已完成。"),
        ],
        true,
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let journal = Arc::new(CloudJournal::open(root.path()).await.unwrap());
    let runtime = CloudRuntime::new(journal.clone());
    let principal = Uuid::new_v4();
    let session = runtime
        .attach(
            principal,
            h.agent.clone(),
            PermissionMode::Default,
            h.broker.clone(),
            None,
        )
        .await
        .unwrap();
    // Hold the actual database writer, so the gateway waiter is definitely
    // disconnected before admission commits. The deployment keeps that future.
    use sqlx::Connection;
    let mut blocker = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(root.path().join("cloud.sqlite")),
    )
    .await
    .unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut blocker)
        .await
        .unwrap();
    let submitting_runtime = runtime.clone();
    let waiter = tokio::spawn(async move {
        submitting_runtime
            .submit(SubmitTurn {
                principal_id: principal,
                session_id: session,
                request_key: "qq:durable-message".into(),
                prompt: MessageContent::text("write once"),
                max_iterations: 8,
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while !runtime.status().active_sessions.contains(&session) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    waiter.abort();
    assert!(matches!(waiter.await, Err(error) if error.is_cancelled()));
    sqlx::query("COMMIT").execute(&mut blocker).await.unwrap();
    let completed = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let state = journal.session(principal, session).await.unwrap();
            if state.revision == 1 {
                break state;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(completed.history.iter().any(|payload| matches!(payload, PersistedPayload::Message(message @ BaseMessage::Ai { .. }) if message.content() == "已完成。")));
    let repeated = runtime
        .submit(SubmitTurn {
            principal_id: principal,
            session_id: session,
            request_key: "qq:durable-message".into(),
            prompt: MessageContent::text("write once"),
            max_iterations: 8,
        })
        .await
        .unwrap();
    assert_eq!(repeated.state, TurnState::Completed);
    assert_eq!(repeated.replies[0].text, "已完成。");
    assert_eq!(
        std::fs::read_to_string(h.workspace.path().join("durable.txt")).unwrap(),
        "only-once"
    );
    assert_eq!(h.script.requests.lock().len(), 2);
    assert!(runtime.shutdown(Duration::from_secs(15)).await.complete);
    h.close().await;
}

#[tokio::test]
async fn bounded_shutdown_retains_background_cleanup_and_retry_proves_device_process_exit() {
    #[cfg(windows)]
    let command = "Start-Sleep -Seconds 30";
    #[cfg(unix)]
    let command = "sleep 30";
    let h = Harness::new(
        vec![
            tool_call(
                "background",
                "Bash",
                json!({"command":command,"run_in_background":true}),
            ),
            answer("后台任务已经启动。"),
        ],
        true,
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let journal = Arc::new(CloudJournal::open(root.path()).await.unwrap());
    let runtime = CloudRuntime::new(journal.clone());
    let principal = Uuid::new_v4();
    let session = runtime
        .attach(
            principal,
            h.agent.clone(),
            PermissionMode::Bypass,
            h.broker.clone(),
            None,
        )
        .await
        .unwrap();
    let receipt = runtime
        .submit(SubmitTurn {
            principal_id: principal,
            session_id: session,
            request_key: "web:background".into(),
            prompt: MessageContent::text("start background job"),
            max_iterations: 8,
        })
        .await
        .unwrap();
    let jobs = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if journal
                .turn(principal, session, receipt.turn_id)
                .await
                .unwrap()
                .state
                == TurnState::Completed
            {
                let state = journal.session(principal, session).await.unwrap();
                break state.unsettled_jobs.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(jobs.len(), 1);
    let incomplete = runtime.shutdown(Duration::ZERO).await;
    assert!(!incomplete.complete);
    assert!(incomplete.pending_sessions.contains(&session));
    let complete = runtime.shutdown(Duration::from_secs(15)).await;
    assert!(complete.complete, "{complete:?}");
    let final_job = h.executor.job(jobs[0].invocation_id).await.unwrap();
    assert_eq!(
        final_job.status,
        peri_acp_types::device_executor::JobStatus::Cancelled
    );
    assert!(final_job.finished_at.is_some());
    assert!(final_job.cancel_requested);
    h.close().await;
}
