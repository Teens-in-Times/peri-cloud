use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{header::CONTENT_TYPE, Method, StatusCode};
use axum::response::Response;
use axum::routing::post;
use axum::{Json, Router};
use parking_lot::Mutex;
use peri_acp_types::device_executor::SubmitJob;
use peri_acp_types::interaction::{InteractionContext, InteractionResponse, UserInteractionBroker};
use peri_acp_types::messages::MessageContent;
use peri_acp_types::permission::{PermissionMode, SharedPermissionMode};
use peri_acp_types::tools::{ToolContext, ToolExecutionStatus, ToolOutput};
use peri_cloud::{CloudAgent, CloudTurnRequest, TurnStatus};
use peri_executor::{web, Executor};
use peri_model::{OpenAiConfig, OpenAiModel};
use peri_remote_tools::{DeviceClient, Error, RemoteSession};
use serde_json::{json, Value};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone, Copy)]
enum LossMode {
    ErrorAfterCommit,
    MalformedAfterCommit,
    ErrorBeforeOwnedAdmission,
    HoldAfterCommit,
}

struct Wire {
    executor: Arc<Executor>,
    origin: String,
    http: reqwest::Client,
    mode: LossMode,
    first: AtomicBool,
    submissions: Mutex<Vec<SubmitJob>>,
    model_requests: Mutex<Vec<Value>>,
    release: Arc<Notify>,
    waiting: Notify,
    delayed: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Wire {
    async fn settled(&self, id: Uuid) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if self.executor.job(id).await.unwrap().status.is_terminal() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
}

async fn proxy(State(wire): State<Arc<Wire>>, request: Request) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let auth = request.headers().get("authorization").unwrap().clone();
    let bytes = to_bytes(request.into_body(), 65536).await.unwrap();
    let request = wire
        .http
        .request(method.clone(), format!("{}{path}", wire.origin))
        .header("authorization", auth)
        .header(CONTENT_TYPE, "application/json")
        .body(bytes.clone());
    let lost = if method == Method::POST && path == "/v1/jobs" {
        wire.submissions
            .lock()
            .push(serde_json::from_slice(&bytes).unwrap());
        wire.first.swap(false, Ordering::SeqCst)
    } else {
        false
    };
    if lost && matches!(wire.mode, LossMode::ErrorBeforeOwnedAdmission) {
        let owner = wire.clone();
        let release = wire.release.clone();
        wire.delayed.lock().push(tokio::spawn(async move {
            release.notified().await;
            let receipt: Value = request
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            owner
                .settled(
                    Uuid::parse_str(receipt["job"]["invocation_id"].as_str().unwrap()).unwrap(),
                )
                .await;
        }));
        return Response::builder()
            .status(StatusCode::GATEWAY_TIMEOUT)
            .body(Body::empty())
            .unwrap();
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    let bytes = response.bytes().await.unwrap();
    if lost {
        assert!(status.is_success());
        let receipt: Value = serde_json::from_slice(&bytes).unwrap();
        wire.settled(Uuid::parse_str(receipt["job"]["invocation_id"].as_str().unwrap()).unwrap())
            .await;
        if matches!(wire.mode, LossMode::HoldAfterCommit) {
            wire.waiting.notify_one();
            wire.release.notified().await;
        }
        return match wire.mode {
            LossMode::ErrorAfterCommit => Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::empty())
                .unwrap(),
            LossMode::MalformedAfterCommit => Response::builder()
                .status(StatusCode::OK)
                .body(Body::from("truncated-json"))
                .unwrap(),
            LossMode::HoldAfterCommit => Response::builder()
                .status(StatusCode::GATEWAY_TIMEOUT)
                .body(Body::empty())
                .unwrap(),
            LossMode::ErrorBeforeOwnedAdmission => unreachable!(),
        };
    }
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .unwrap()
}

async fn model(
    State(wire): State<Arc<Wire>>,
    Json(request): Json<Value>,
) -> ([(axum::http::HeaderName, &'static str); 1], String) {
    let index = {
        let mut requests = wire.model_requests.lock();
        let index = requests.len();
        requests.push(request);
        index
    };
    let call = match index {
        0 | 1 => Some((
            format!("distinct-provider-call-{index}"),
            "Write",
            json!({"file_path":"once.txt","content":"once","append":true}),
        )),
        2 => {
            let id = wire.submissions.lock()[0].invocation_id;
            Some((
                "query-original".into(),
                "GetExecution",
                json!({"task_id":id}),
            ))
        }
        3 => Some((
            "read-after-confirmation".into(),
            "Read",
            json!({"file_path":"once.txt"}),
        )),
        4 => None,
        _ => panic!("unexpected model request"),
    };
    let delta = match call {
        Some((id, name, input)) => {
            json!({"choices":[{"delta":{"role":"assistant","tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}}]},"finish_reason":"tool_calls"}]})
        }
        None => {
            json!({"choices":[{"delta":{"role":"assistant","content":"已经确认原任务，只写入一次。"},"finish_reason":"stop"}]})
        }
    };
    (
        [(CONTENT_TYPE, "text/event-stream")],
        format!("data: {delta}\n\ndata: [DONE]\n\n"),
    )
}

struct NoApproval;
#[async_trait]
impl UserInteractionBroker for NoApproval {
    async fn request(&self, _: InteractionContext) -> InteractionResponse {
        panic!("existing Bypass policy should not ask for approval")
    }
}

struct Fixture {
    _state: tempfile::TempDir,
    workspace: tempfile::TempDir,
    wire: Arc<Wire>,
    session: Arc<RemoteSession>,
    agent: CloudAgent,
    stop: CancellationToken,
    servers: Vec<tokio::task::JoinHandle<()>>,
}

impl Fixture {
    async fn new(mode: LossMode) -> Self {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let executor = Arc::new(
            Executor::open(state.path(), "uncertainty-fixture")
                .await
                .unwrap(),
        );
        let token = std::fs::read_to_string(state.path().join("transport-token")).unwrap();
        let stop = CancellationToken::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let app = web::router(executor.clone());
        let signal = stop.clone();
        let executor_server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(signal.cancelled_owned())
                .await
                .unwrap();
        });
        let wire = Arc::new(Wire {
            executor,
            origin,
            mode,
            first: AtomicBool::new(true),
            http: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            submissions: Mutex::new(Vec::new()),
            model_requests: Mutex::new(Vec::new()),
            release: Arc::new(Notify::new()),
            waiting: Notify::new(),
            delayed: Mutex::new(Vec::new()),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .fallback(proxy)
            .route("/v1/chat/completions", post(model))
            .with_state(wire.clone());
        let signal = stop.clone();
        let wire_server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(signal.cancelled_owned())
                .await
                .unwrap();
        });
        let client = DeviceClient::connect(address, token, wire.executor.info().device_id)
            .await
            .unwrap();
        let cloud_session = Uuid::new_v4();
        let session = RemoteSession::open(
            client,
            Uuid::new_v4(),
            cloud_session.to_string(),
            workspace.path().to_str().unwrap(),
        )
        .await
        .unwrap();
        let model = Arc::new(OpenAiModel::new(OpenAiConfig::new(
            format!("http://{address}/v1").parse().unwrap(),
            "fixture-model-key",
            "fixture-model",
        )));
        let agent = CloudAgent::new(
            cloud_session,
            session.clone(),
            model,
            "Work on the bound device. Query the original task after an unknown result.".into(),
            128000,
        )
        .unwrap();
        Self {
            _state: state,
            workspace,
            wire,
            session,
            agent,
            stop,
            servers: vec![executor_server, wire_server],
        }
    }

    async fn invoke(
        &self,
        name: &str,
        input: Value,
        call: &str,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, Box<dyn std::error::Error + Send + Sync>> {
        let mut context = ToolContext::new(&[], "cloud-path-is-not-a-workspace");
        context.session_id = Some(self.session.cloud_session_id().into());
        context.turn_generation = Some("uncertainty-turn".into());
        context.invocation_id = Some(call.into());
        context.cancellation = cancel;
        self.session
            .tools()
            .into_iter()
            .find(|tool| tool.name() == name)
            .unwrap()
            .invoke_output(input, context)
            .await
    }

    async fn finish(self) {
        let delayed = std::mem::take(&mut *self.wire.delayed.lock());
        for task in delayed {
            task.await.unwrap();
        }
        self.wire.executor.shutdown().await.unwrap();
        self.stop.cancel();
        for server in self.servers {
            server.await.unwrap();
        }
    }
}

#[tokio::test]
async fn original_loop_blocks_new_provider_id_after_lost_submit_until_original_task_is_queried() {
    for mode in [LossMode::ErrorAfterCommit, LossMode::MalformedAfterCommit] {
        let f = Fixture::new(mode).await;
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            f.agent.run_turn(CloudTurnRequest {
                turn_id: Uuid::new_v4(),
                prompt: MessageContent::Text("追加一次内容".into()),
                history: Vec::new(),
                history_flags: HashMap::new(),
                broker: Arc::new(NoApproval),
                permissions: SharedPermissionMode::new(PermissionMode::Bypass),
                classifier: None,
                cancellation: CancellationToken::new(),
                max_iterations: 8,
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.status, TurnStatus::Completed);
        assert!(result.unsettled_jobs.unwrap().is_empty());
        assert_eq!(result.replies[0].text, "已经确认原任务，只写入一次。");
        assert_eq!(
            std::fs::read_to_string(f.workspace.path().join("once.txt")).unwrap(),
            "once"
        );
        {
            let submissions = f.wire.submissions.lock();
            assert_eq!(
                submissions.len(),
                2,
                "one Write plus one Read; the second Write must never reach the executor"
            );
            assert_eq!(submissions[0].tool, "Write");
            assert_eq!(submissions[1].tool, "Read");
            let requests = f.wire.model_requests.lock();
            assert_eq!(requests.len(), 5);
            assert!(requests[2]["messages"]
                .to_string()
                .contains("This new operation was not submitted"));
            assert!(requests[2]["messages"]
                .to_string()
                .contains(&submissions[0].invocation_id.to_string()));
        }
        assert_eq!(f.session.jobs().await.unwrap().len(), 2);
        f.finish().await;
    }
}

#[tokio::test]
async fn missing_job_does_not_settle_owned_admission_or_allow_changed_input_and_cancel_is_not_exit()
{
    let f = Fixture::new(LossMode::ErrorBeforeOwnedAdmission).await;
    let input = json!({"file_path":"once.txt","content":"once","append":true});
    let output = f
        .invoke("Write", input.clone(), "first", CancellationToken::new())
        .await
        .unwrap();
    let evidence = output.execution.unwrap();
    assert_eq!(evidence.status, ToolExecutionStatus::Unknown);
    let id = Uuid::parse_str(evidence.task_id.as_ref().unwrap()).unwrap();
    assert!(
        matches!(f.session.jobs().await, Err(Error::UnconfirmedExecution)),
        "an empty executor list cannot prove that an in-flight request was not admitted"
    );
    let query = f
        .invoke(
            "GetExecution",
            json!({"task_id":id}),
            "query",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        query.downcast_ref::<Error>(),
        Some(Error::Rejected(404))
    ));
    let blocked = f
        .invoke(
            "Write",
            input.clone(),
            "new-provider-id",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(blocked.execution.unwrap().task_id, Some(id.to_string()));
    let changed = f
        .invoke(
            "Write",
            json!({"file_path":"once.txt","content":"changed","append":true}),
            "first",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        changed.downcast_ref::<Error>(),
        Some(Error::Rejected(409))
    ));
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let cancel = f.invoke("Write", input, "first", cancelled).await.unwrap();
    assert_eq!(
        cancel.execution.unwrap().status,
        ToolExecutionStatus::Unknown,
        "a cancelled retry does not settle the original submission"
    );
    assert_eq!(f.wire.submissions.lock().len(), 1);
    assert!(!f.workspace.path().join("once.txt").exists());
    f.wire.release.notify_one();
    let delayed = std::mem::take(&mut *f.wire.delayed.lock());
    for task in delayed {
        task.await.unwrap();
    }
    let settled = f
        .invoke(
            "GetExecution",
            json!({"task_id":id}),
            "query-after-admission",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        settled.execution.unwrap().status,
        ToolExecutionStatus::Completed
    );
    let read = f
        .invoke(
            "Read",
            json!({"file_path":"once.txt"}),
            "read-after-query",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(read.text.contains("once"));
    assert_eq!(
        std::fs::read_to_string(f.workspace.path().join("once.txt")).unwrap(),
        "once"
    );
    assert_eq!(f.wire.submissions.lock().len(), 2);
    f.finish().await;
}

#[tokio::test]
async fn aborted_tool_waiter_retains_admission_before_the_receipt_can_return() {
    let f = Fixture::new(LossMode::HoldAfterCommit).await;
    let session = f.session.clone();
    let waiter = tokio::spawn(async move {
        let mut context = ToolContext::new(&[], "not-the-device-workspace");
        context.session_id = Some(session.cloud_session_id().into());
        context.turn_generation = Some("aborted-turn".into());
        context.invocation_id = Some("aborted-call".into());
        session
            .tools()
            .into_iter()
            .find(|tool| tool.name() == "Write")
            .unwrap()
            .invoke_output(
                json!({"file_path":"once.txt","content":"once","append":true}),
                context,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), f.wire.waiting.notified())
        .await
        .unwrap();
    let original = f.wire.submissions.lock()[0].invocation_id;
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    f.wire.release.notify_one();
    let blocked = f
        .invoke(
            "Write",
            json!({"file_path":"once.txt","content":"once","append":true}),
            "new-call",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        blocked.execution.unwrap().task_id,
        Some(original.to_string())
    );
    assert_eq!(f.wire.submissions.lock().len(), 1);
    assert_eq!(
        std::fs::read_to_string(f.workspace.path().join("once.txt")).unwrap(),
        "once"
    );
    let query = f
        .invoke(
            "GetExecution",
            json!({"task_id":original}),
            "query-aborted",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        query.execution.unwrap().status,
        ToolExecutionStatus::Completed
    );
    let read = f
        .invoke(
            "Read",
            json!({"file_path":"once.txt"}),
            "after-aborted-confirmation",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(read.text.contains("once"));
    assert_eq!(f.wire.submissions.lock().len(), 2);
    f.finish().await;
}
