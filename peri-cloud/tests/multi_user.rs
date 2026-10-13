use async_trait::async_trait;
use futures::stream;
use parking_lot::Mutex;
use peri_acp_types::{
    interaction::{InteractionContext, InteractionResponse, UserInteractionBroker},
    messages::MessageContent,
    permission::PermissionMode,
};
use peri_cloud::{
    gateway::{NativeSessionConnector, SessionConnector},
    identity::DeviceRecord,
    limits::RuntimeLimits,
    state::{CloudJournal, StateError, TurnState},
    CloudRuntime, RuntimeError, SubmitTurn,
};
use peri_executor::Executor;
use peri_model::{
    Model, ModelCapabilities, ModelError, ModelMessage, ModelRequest, ModelResponse, ModelResult,
    ModelStream, ModelStreamEvent, StopReason,
};
use peri_remote_tools::DeviceClient;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct ModelGate {
    calls: AtomicUsize,
    release: tokio::sync::Semaphore,
    prompts: Mutex<Vec<String>>,
}
#[async_trait]
impl Model for ModelGate {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            supports_streaming: true,
            ..Default::default()
        }
    }
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> ModelResult<ModelStream> {
        self.prompts
            .lock()
            .push(serde_json::to_string(&request).unwrap());
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::select! { _=cancel.cancelled() => return Err(ModelError::cancelled()), permit=self.release.acquire() => permit.unwrap().forget() }
        let response = ModelResponse::new(
            ModelMessage::assistant_text("done"),
            StopReason::EndTurn,
            None,
            None,
        )
        .unwrap();
        Ok(ModelStream::with_parent_cancellation(
            stream::iter(vec![Ok(ModelStreamEvent::Completed(response))]),
            cancel,
        ))
    }
}
struct Broker;
#[async_trait]
impl UserInteractionBroker for Broker {
    async fn request(&self, _context: InteractionContext) -> InteractionResponse {
        InteractionResponse::Rejected
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    journal: Arc<CloudJournal>,
    runtime: Arc<CloudRuntime>,
    connector: Arc<NativeSessionConnector>,
    executor: Arc<Executor>,
    device: DeviceRecord,
    model: Arc<ModelGate>,
    stop: CancellationToken,
    server: tokio::task::JoinHandle<()>,
}
impl Fixture {
    async fn new(limits: RuntimeLimits) -> Self {
        let root = tempfile::tempdir().unwrap();
        let journal = Arc::new(
            CloudJournal::open(&root.path().join("cloud"))
                .await
                .unwrap(),
        );
        let runtime = CloudRuntime::with_limits(journal.clone(), limits).unwrap();
        let executor = Arc::new(
            Executor::open(&root.path().join("executor"), "Fixture")
                .await
                .unwrap(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let stop = CancellationToken::new();
        let cancelled = stop.clone();
        let app = peri_executor::web::router(executor.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(cancelled.cancelled_owned())
                .await
                .unwrap();
        });
        let token = std::fs::read_to_string(root.path().join("executor/transport-token")).unwrap();
        let client = DeviceClient::connect(address, token, executor.info().device_id)
            .await
            .unwrap();
        let model = Arc::new(ModelGate {
            calls: AtomicUsize::new(0),
            release: tokio::sync::Semaphore::new(0),
            prompts: Mutex::new(Vec::new()),
        });
        let connector =
            NativeSessionConnector::new(model.clone(), "fixture cloud rules".into(), 8192).unwrap();
        connector.register_client(client);
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let device = DeviceRecord {
            id: executor.info().device_id,
            name: "Fixture".into(),
            platform: std::env::consts::OS.into(),
            default_workspace: workspace.to_str().unwrap().into(),
            connection_id: None,
            is_local: false,
            revoked: false,
        };
        Self {
            _root: root,
            journal,
            runtime,
            connector,
            executor,
            device,
            model,
            stop,
            server,
        }
    }
    async fn attach(&self, principal: Uuid) -> Uuid {
        let agent = self
            .connector
            .open(principal, &self.device, None)
            .await
            .unwrap();
        self.runtime
            .attach(
                principal,
                agent,
                PermissionMode::Default,
                Arc::new(Broker),
                None,
            )
            .await
            .unwrap()
    }
    async fn submit(
        &self,
        principal: Uuid,
        session: Uuid,
        key: &str,
    ) -> Result<peri_cloud::state::TurnRecord, RuntimeError> {
        self.runtime
            .submit(SubmitTurn {
                principal_id: principal,
                session_id: session,
                request_key: key.into(),
                prompt: MessageContent::text("hello"),
                max_iterations: 4,
            })
            .await
    }
    async fn wait_state(&self, principal: Uuid, session: Uuid, turn: Uuid, state: TurnState) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if self
                    .journal
                    .turn(principal, session, turn)
                    .await
                    .unwrap()
                    .state
                    == state
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    async fn finish(self) {
        assert!(
            self.runtime
                .shutdown(Duration::from_secs(10))
                .await
                .complete
        );
        self.executor.shutdown().await.unwrap();
        self.stop.cancel();
        self.server.await.unwrap();
    }
}

#[tokio::test]
async fn different_users_run_concurrently_with_global_per_user_and_daily_limits() {
    let f = Fixture::new(RuntimeLimits {
        max_active_turns: 2,
        max_active_per_user: 1,
        daily_turns: 1,
        ..Default::default()
    })
    .await;
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let c = Uuid::new_v4();
    let sa = f.attach(a).await;
    let sa2 = f.attach(a).await;
    let sb = f.attach(b).await;
    let sc = f.attach(c).await;
    let ta = f.submit(a, sa, "a").await.unwrap();
    assert!(matches!(
        f.submit(a, sa2, "a2").await,
        Err(RuntimeError::State(StateError::Limit))
    ));
    let tb = f.submit(b, sb, "b").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while f.model.calls.load(Ordering::SeqCst) != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        f.submit(c, sc, "c").await,
        Err(RuntimeError::State(StateError::Limit))
    ));
    assert!(matches!(
        f.submit(b, sa, "foreign").await,
        Err(RuntimeError::State(StateError::Forbidden))
    ));
    assert_eq!(f.submit(a, sa, "a").await.unwrap().turn_id, ta.turn_id);
    f.model.release.add_permits(2);
    f.wait_state(a, sa, ta.turn_id, TurnState::Completed).await;
    f.wait_state(b, sb, tb.turn_id, TurnState::Completed).await;
    assert!(matches!(
        f.submit(a, sa2, "daily").await,
        Err(RuntimeError::State(StateError::Limit))
    ));
    assert_eq!(f.journal.user_usage(a).await.unwrap().turns, 1);
    assert_eq!(f.journal.user_usage(a).await.unwrap().model_requests, 1);
    assert_eq!(f.journal.user_usage(b).await.unwrap().turns, 1);
    assert_eq!(f.journal.user_usage(c).await.unwrap().turns, 0);
    f.finish().await;
}

#[tokio::test]
async fn turn_deadline_cancels_owned_model_work_and_retains_a_settled_receipt() {
    let f = Fixture::new(RuntimeLimits {
        max_turn_seconds: 1,
        ..Default::default()
    })
    .await;
    let principal = Uuid::new_v4();
    let session = f.attach(principal).await;
    let turn = f.submit(principal, session, "deadline").await.unwrap();
    f.wait_state(principal, session, turn.turn_id, TurnState::Interrupted)
        .await;
    assert_eq!(
        f.submit(principal, session, "deadline")
            .await
            .unwrap()
            .turn_id,
        turn.turn_id
    );
    assert!(f
        .journal
        .session(principal, session)
        .await
        .unwrap()
        .execution_settled());
    f.finish().await;
}
