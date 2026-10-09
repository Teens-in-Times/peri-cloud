use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use peri_acp_types::device_executor::{JobStatus, OpenSession, SubmitJob};
use peri_acp_types::tools::{ToolContext, ToolExecutionStatus, ToolOutput};
use peri_executor::{web, Executor};
use peri_remote_tools::{DeviceClient, Error, RemoteSession};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct Harness {
    _state: tempfile::TempDir,
    workspace: tempfile::TempDir,
    executor: Arc<Executor>,
    client: Arc<DeviceClient>,
    session: Arc<RemoteSession>,
    token: String,
    address: SocketAddr,
    stop: CancellationToken,
    server: tokio::task::JoinHandle<()>,
}

async fn serve(
    executor: Arc<Executor>,
) -> (SocketAddr, CancellationToken, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stop = CancellationToken::new();
    let signal = stop.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, web::router(executor))
            .with_graceful_shutdown(signal.cancelled_owned())
            .await
            .unwrap();
    });
    (address, stop, server)
}

impl Harness {
    async fn new() -> Self {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let executor = Arc::new(Executor::open(state.path(), "adapter-test").await.unwrap());
        let token = std::fs::read_to_string(state.path().join("transport-token")).unwrap();
        let (address, stop, server) = serve(executor.clone()).await;
        let client = DeviceClient::connect(address, token.clone(), executor.info().device_id)
            .await
            .unwrap();
        let session = RemoteSession::open(
            client.clone(),
            Uuid::new_v4(),
            "cloud-session".into(),
            workspace.path().to_str().unwrap(),
        )
        .await
        .unwrap();
        Self {
            _state: state,
            workspace,
            executor,
            client,
            session,
            token,
            address,
            stop,
            server,
        }
    }

    async fn invoke(&self, name: &str, input: Value, call: &str, turn: &str) -> ToolOutput {
        self.session
            .tools()
            .into_iter()
            .find(|tool| tool.name() == name)
            .unwrap()
            .invoke_output(input, context("cloud-session", turn, call))
            .await
            .unwrap()
    }

    async fn terminal(&self, id: Uuid) -> ToolOutput {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let output = self
                    .invoke("GetExecution", json!({"task_id": id}), "query", "turn")
                    .await;
                if !matches!(
                    output.execution.as_ref().unwrap().status,
                    ToolExecutionStatus::Running
                ) {
                    return output;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap()
    }

    async fn close(self) {
        self.stop.cancel();
        self.server.await.unwrap();
        self.executor.shutdown().await.unwrap();
    }
}

fn context<'a>(session: &str, turn: &str, call: &str) -> ToolContext<'a> {
    let mut context = ToolContext::new(&[], "host-must-not-resolve-remote-paths");
    context.session_id = Some(session.into());
    context.turn_generation = Some(turn.into());
    context.invocation_id = Some(call.into());
    context
}

#[tokio::test]
async fn native_parameters_and_metadata_survive_adaptation_and_write_retry_does_not_append_twice() {
    let h = Harness::new().await;
    let descriptors = h
        .client
        .catalog(h.session.binding().session_id)
        .await
        .unwrap();
    let tools = h.session.tools();
    for descriptor in descriptors {
        let tool = tools
            .iter()
            .find(|tool| tool.name() == descriptor.definition.name)
            .unwrap();
        assert_eq!(
            tool.definition().parameters,
            descriptor.definition.parameters
        );
        assert_eq!(tool.description(), descriptor.definition.description);
        assert_eq!(tool.namespace(), descriptor.namespace.as_deref());
        assert_eq!(tool.prefers_persist(), descriptor.prefers_persist);
        assert_eq!(tool.context_retention(), descriptor.context_retention);
        assert!(!tool.parameters().to_string().contains("ssh"));
    }
    let input = json!({"file_path":"note.txt","content":"one","append":true});
    let first = h
        .invoke("Write", input.clone(), "provider-call-1", "turn-1")
        .await;
    let retry = h.invoke("Write", input, "provider-call-1", "turn-1").await;
    assert_eq!(first, retry);
    assert_eq!(
        std::fs::read_to_string(h.workspace.path().join("note.txt")).unwrap(),
        "one"
    );
    let write = tools.iter().find(|tool| tool.name() == "Write").unwrap();
    let changed = write
        .invoke_output(
            json!({"file_path":"note.txt","content":"two","append":true}),
            context("cloud-session", "turn-1", "provider-call-1"),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        changed.downcast_ref::<Error>(),
        Some(Error::Rejected(409))
    ));
    let read = h
        .invoke("Read", json!({"file_path":"note.txt"}), "read", "turn-1")
        .await;
    assert!(read.text.contains("one"));
    h.invoke(
        "Write",
        json!({"file_path":"note.txt","content":"two","append":true}),
        "provider-call-1",
        "turn-2",
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(h.workspace.path().join("note.txt")).unwrap(),
        "onetwo"
    );
    assert_eq!(h.session.jobs().await.unwrap().len(), 3);
    h.close().await;
}

#[tokio::test]
async fn wrong_device_and_missing_or_cross_session_identity_fail_before_execution() {
    let h = Harness::new().await;
    let wrong = DeviceClient::connect(h.address, h.token.clone(), Uuid::new_v4())
        .await
        .err()
        .unwrap();
    assert!(matches!(wrong, Error::DeviceMismatch));
    let nonlocal = DeviceClient::connect(
        "192.0.2.1:1234".parse().unwrap(),
        h.token.clone(),
        h.executor.info().device_id,
    )
    .await
    .err()
    .unwrap();
    assert!(matches!(nonlocal, Error::NonLoopback));
    let tools = h.session.tools();
    let write = tools.iter().find(|tool| tool.name() == "Write").unwrap();
    let input = json!({"file_path":"must-not-exist.txt","content":"bad"});
    let missing = write
        .invoke_output(input.clone(), ToolContext::new(&[], "unused"))
        .await
        .unwrap_err();
    assert!(matches!(
        missing.downcast_ref::<Error>(),
        Some(Error::MissingIdentity)
    ));
    let cross = write
        .invoke_output(input, context("other-session", "turn", "call"))
        .await
        .unwrap_err();
    assert!(matches!(
        cross.downcast_ref::<Error>(),
        Some(Error::SessionMismatch)
    ));
    assert!(!h.workspace.path().join("must-not-exist.txt").exists());
    assert!(h.session.jobs().await.unwrap().is_empty());
    h.close().await;
}

#[tokio::test]
async fn disconnected_shell_is_recovered_on_a_new_endpoint_without_reexecution() {
    let mut h = Harness::new().await;
    #[cfg(windows)]
    let command = "Start-Sleep -Seconds 2; [IO.File]::AppendAllText('once.txt', 'once')";
    #[cfg(unix)]
    let command = "sleep 2; printf once >> once.txt";
    let input = json!({"command":command,"run_in_background":true});
    let running = h
        .invoke("Bash", input.clone(), "background-call", "turn")
        .await;
    let id = Uuid::parse_str(running.execution.unwrap().task_id.as_ref().unwrap()).unwrap();
    h.stop.cancel();
    (&mut h.server).await.unwrap();
    let uncertain = h
        .invoke("Bash", input.clone(), "background-call", "turn")
        .await;
    assert_eq!(
        uncertain.execution.unwrap().status,
        ToolExecutionStatus::Unknown
    );
    let (address, stop, server) = serve(h.executor.clone()).await;
    h.client.reconnect(address).await.unwrap();
    h.address = address;
    h.stop = stop;
    h.server = server;
    let completed = h.terminal(id).await;
    assert_eq!(
        completed.execution.as_ref().unwrap().status,
        ToolExecutionStatus::Completed
    );
    let retried = h.invoke("Bash", input, "background-call", "turn").await;
    assert_eq!(completed, retried);
    assert_eq!(
        std::fs::read_to_string(h.workspace.path().join("once.txt")).unwrap(),
        "once"
    );
    assert_eq!(h.session.jobs().await.unwrap().len(), 1);
    h.close().await;
}

#[tokio::test]
async fn foreground_wait_deadline_returns_running_and_control_tools_report_actual_cancellation() {
    let h = Harness::new().await;
    #[cfg(windows)]
    let command = "Start-Sleep -Seconds 30";
    #[cfg(unix)]
    let command = "sleep 30";
    let output = h
        .invoke(
            "Bash",
            json!({"command":command,"timeout":1}),
            "long-shell",
            "turn",
        )
        .await;
    let evidence = output.execution.unwrap();
    assert_eq!(evidence.status, ToolExecutionStatus::RunningAfterTimeout);
    let id = Uuid::parse_str(evidence.task_id.as_ref().unwrap()).unwrap();
    let requested = h
        .invoke("CancelExecution", json!({"task_id":id}), "cancel", "turn")
        .await;
    assert!(matches!(
        requested.execution.unwrap().status,
        ToolExecutionStatus::Running | ToolExecutionStatus::Cancelled
    ));
    assert_eq!(
        h.terminal(id).await.execution.unwrap().status,
        ToolExecutionStatus::Cancelled
    );
    h.close().await;
}

#[tokio::test]
async fn task_control_checks_bound_session_before_cancelling_other_sessions_job() {
    let h = Harness::new().await;
    let other = Uuid::new_v4();
    h.executor
        .bind_session(
            other,
            OpenSession {
                workspace: h.workspace.path().to_str().unwrap().into(),
            },
        )
        .await
        .unwrap();
    #[cfg(windows)]
    let command = "Start-Sleep -Seconds 30";
    #[cfg(unix)]
    let command = "sleep 30";
    let id = Uuid::new_v4();
    h.executor
        .submit(SubmitJob {
            invocation_id: id,
            session_id: other,
            tool: "Bash".into(),
            input: json!({"command":command}),
        })
        .await
        .unwrap();
    let tools = h.session.tools();
    let cancel = tools
        .iter()
        .find(|tool| tool.name() == "CancelExecution")
        .unwrap();
    let error = cancel
        .invoke_output(
            json!({"task_id":id}),
            context("cloud-session", "turn", "cancel-other"),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<Error>(),
        Some(Error::Protocol)
    ));
    let job = h.executor.job(id).await.unwrap();
    assert!(!job.cancel_requested);
    assert!(matches!(job.status, JobStatus::Queued | JobStatus::Running));
    h.close().await;
}
