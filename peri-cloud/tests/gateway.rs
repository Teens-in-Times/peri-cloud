use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::routing::post;
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use parking_lot::Mutex;
use peri_cloud::gateway::{
    ChannelRoute, Delivery, DeliveryBody, DeliveryError, Gateway, GatewayError, InboundMessage,
    InteractionAction, InteractionCard, MessageAdapter, NativeSessionConnector, ReceiptState,
    WebAdapter,
};
use peri_cloud::identity::{
    Authenticated, ChannelIdentity, DeviceRecord, IdentityClock, IdentityError, IdentityService,
    NativeAuthorization,
};
use peri_cloud::state::{CloudJournal, TurnState};
use peri_cloud::CloudRuntime;
use peri_executor::{web, Executor};
use peri_model::{OpenAiConfig, OpenAiModel};
use peri_remote_tools::DeviceClient;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const SETUP: &str = "fixture-cloud-setup-at-least-32-characters";
const PASSWORD: &str = "fixture-cloud-owner-password-for-argon2";
const VERIFIER: &str = "fixture-native-pkce-verifier-with-at-least-43-characters";
#[path = "gateway/access.rs"]
mod access_tests;
#[path = "gateway/commands.rs"]
mod command_tests;
#[path = "gateway/execution_sessions.rs"]
mod execution_session_tests;
#[path = "gateway/experience.rs"]
mod experience_tests;
#[path = "gateway/memory.rs"]
mod memory_tests;
#[path = "gateway/menu_details.rs"]
mod menu_detail_tests;
#[path = "gateway/menus.rs"]
mod menu_tests;
#[path = "gateway/schedules.rs"]
mod schedule_tests;
#[path = "gateway/topology.rs"]
mod topology_tests;
#[path = "gateway/web_tasks.rs"]
mod web_task_tests;
#[path = "gateway/workspaces.rs"]
mod workspace_tests;

struct Clock(AtomicI64);
impl IdentityClock for Clock {
    fn unix_seconds(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Default)]
struct Script {
    replies: Mutex<VecDeque<String>>,
    requests: Mutex<Vec<Value>>,
    review_requests: Mutex<Vec<Value>>,
    review_replies: Mutex<VecDeque<String>>,
    hold_review: AtomicBool,
    review_waiting: Notify,
    review_release: Notify,
    dynamic: Mutex<Option<DynamicResponse>>,
}
type DynamicResponse = Box<dyn FnMut(&Value) -> String + Send>;

async fn model_request(
    State(script): State<Arc<Script>>,
    Json(request): Json<Value>,
) -> ([(axum::http::HeaderName, &'static str); 1], String) {
    let control = request["tools"].as_array().is_some_and(|tools| {
        !tools.is_empty()
            && tools.iter().all(|tool| {
                matches!(
                    tool["function"]["name"].as_str(),
                    Some("decide_approval" | "inspect_task" | "message_task")
                )
            })
    });
    if control {
        let pending = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find_map(|message| {
                let text = message["content"].as_str()?;
                let (_, body) =
                    text.split_once("Current task observations (untrusted content):\n")?;
                let body = body
                    .replace("&quot;", "\"")
                    .replace("&apos;", "'")
                    .replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .replace("&amp;", "&");
                serde_json::Deserializer::from_str(&body)
                    .into_iter::<Value>()
                    .next()?
                    .ok()
                    .and_then(|v| v["pending_approvals"].as_array()?.first().cloned())
            });
        let Some(pending) = pending else {
            return (
                [(CONTENT_TYPE, "text/event-stream")],
                answer("No pending approval."),
            );
        };
        script.review_requests.lock().push(request);
        if script.hold_review.load(Ordering::SeqCst) {
            script.review_waiting.notify_one();
            script.review_release.notified().await;
        }
        // The mock emits a typed tool call. Legacy scenario fixtures only supply
        // decision values; production never parses free-form assistant approval.
        let scripted = script.review_replies.lock().pop_front();
        let mut decision = scripted
            .and_then(|reply| {
                let body = reply.strip_prefix("data: ")?.split("\n").next()?;
                let delta: Value = serde_json::from_str(body).ok()?;
                serde_json::from_str::<Value>(delta["choices"][0]["delta"]["content"].as_str()?)
                    .ok()
            })
            .unwrap_or_else(
                || json!({"approve":true,"deletion_possible":false,"reason":"within user task"}),
            );
        decision.as_object_mut().unwrap().remove("notice");
        decision["request_id"] = pending["request_id"].clone();
        return (
            [(CONTENT_TYPE, "text/event-stream")],
            execution_session_tests::call("decide_approval", decision, "typed-review"),
        );
    }
    script.requests.lock().push(request.clone());
    if let Some(responder) = script.dynamic.lock().as_mut() {
        return ([(CONTENT_TYPE, "text/event-stream")], responder(&request));
    }
    let reply = script.replies.lock().pop_front().expect("模型调用超出脚本");
    ([(CONTENT_TYPE, "text/event-stream")], reply)
}

fn tool_write() -> String {
    let delta = json!({"choices":[{"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"fixture-write-call","type":"function","function":{"name":"Write","arguments":json!({"file_path":"agent.txt","content":"PRIVATE_TOOL_CONTENT"}).to_string()}}]},"finish_reason":"tool_calls"}]});
    format!("data: {delta}\n\ndata: [DONE]\n\n")
}

fn answer(text: &str) -> String {
    let delta = json!({"choices":[{"delta":{"role":"assistant","content":text,"reasoning_content":"PRIVATE_REASONING"},"finish_reason":"stop"}]});
    format!("data: {delta}\n\ndata: [DONE]\n\n")
}

#[derive(Default)]
struct Adapter {
    attempts: Mutex<Vec<Delivery>>,
    fail_next: AtomicU8,
    block_next: AtomicBool,
    panic_next: AtomicBool,
    blocked: Notify,
    release: Notify,
}

#[async_trait]
impl MessageAdapter for Adapter {
    fn supports_proactive(&self) -> bool {
        true
    }
    fn conversation_scope(
        &self,
        route: &ChannelRoute,
    ) -> peri_cloud::gateway::access::ConversationScope {
        use peri_cloud::gateway::access::ConversationScope;
        if route.conversation_id == "c2c-owner" {
            ConversationScope::Private
        } else if let Some(group) = route.conversation_id.strip_prefix("group:") {
            ConversationScope::Group(group.into())
        } else {
            ConversationScope::Unknown
        }
    }
    fn initial_access_policy(&self) -> peri_cloud::gateway::access::AccessPolicy {
        peri_cloud::gateway::access::AccessPolicy::open()
    }
    fn instance_id(&self) -> &str {
        "fixture-qq"
    }
    async fn deliver(&self, delivery: &Delivery) -> Result<(), DeliveryError> {
        self.attempts.lock().push(delivery.clone());
        assert!(
            !self.panic_next.swap(false, Ordering::SeqCst),
            "fixture delivery panic"
        );
        if self.block_next.swap(false, Ordering::SeqCst) {
            self.blocked.notify_one();
            self.release.notified().await;
        }
        match self.fail_next.swap(0, Ordering::SeqCst) {
            1 => Err(DeliveryError::Rejected),
            2 => Err(DeliveryError::Unconfirmed),
            _ => Ok(()),
        }
    }
}

struct CloudSide {
    journal: Arc<CloudJournal>,
    identity: Arc<IdentityService>,
    browser: Authenticated,
    runtime: Arc<CloudRuntime>,
    gateway: Arc<Gateway>,
}

impl CloudSide {
    async fn start(
        root: &std::path::Path,
        clock: Arc<Clock>,
        connector: Arc<dyn peri_cloud::gateway::SessionConnector>,
        adapter: Arc<Adapter>,
        initialize: bool,
        web: bool,
    ) -> Self {
        Self::start_with_limits(
            root,
            clock,
            connector,
            adapter,
            initialize,
            web,
            Default::default(),
        )
        .await
    }

    async fn start_with_limits(
        root: &std::path::Path,
        clock: Arc<Clock>,
        connector: Arc<dyn peri_cloud::gateway::SessionConnector>,
        adapter: Arc<Adapter>,
        initialize: bool,
        web: bool,
        limits: peri_cloud::limits::RuntimeLimits,
    ) -> Self {
        let journal = Arc::new(CloudJournal::open(root).await.unwrap());
        let identity = IdentityService::with_clock(&journal, SETUP, clock)
            .await
            .unwrap();
        if initialize {
            identity
                .initialize(SETUP, "owner", PASSWORD.into(), "Owner")
                .await
                .unwrap();
        }
        let login = identity.login("owner", PASSWORD.into()).await.unwrap();
        let browser = identity
            .authenticate_browser(&login.session_token)
            .await
            .unwrap();
        let runtime = CloudRuntime::with_limits(journal.clone(), limits).unwrap();
        let mut adapters: Vec<Arc<dyn MessageAdapter>> = vec![adapter];
        if web {
            adapters.push(Arc::new(WebAdapter::from_journal(&journal)));
        }
        let gateway = Gateway::new(runtime.clone(), identity.clone(), connector, adapters, 8)
            .await
            .unwrap();
        Self {
            journal,
            identity,
            browser,
            runtime,
            gateway,
        }
    }

    async fn stop(&self) {
        assert!(
            self.gateway.shutdown(Duration::from_secs(3)).await.unwrap(),
            "渠道任务须排空"
        );
        assert!(
            self.runtime.shutdown(Duration::from_secs(5)).await.complete,
            "云运行及设备任务须结算"
        );
    }
}

struct Fixture {
    root: tempfile::TempDir,
    cloud: Option<CloudSide>,
    clock: Arc<Clock>,
    connector: Arc<NativeSessionConnector>,
    devices: Vec<DeviceRecord>,
    workspaces: Vec<tempfile::TempDir>,
    executors: Vec<Arc<Executor>>,
    _executor_roots: Vec<tempfile::TempDir>,
    adapter: Arc<Adapter>,
    script: Arc<Script>,
    stop: CancellationToken,
    servers: Vec<tokio::task::JoinHandle<()>>,
    route: ChannelRoute,
    limits: peri_cloud::limits::RuntimeLimits,
    web: bool,
    submissions: Vec<Arc<std::sync::atomic::AtomicUsize>>,
    hidden_receipts: Vec<Arc<AtomicBool>>,
    crash_config: Value,
    held_submissions: Vec<Arc<AtomicBool>>,
    submission_release: Vec<Arc<Notify>>,
}

impl Fixture {
    async fn new(responses: Vec<String>) -> Self {
        Self::new_for(responses, false).await
    }
    async fn new_for(responses: Vec<String>, web: bool) -> Self {
        Self::new_for_limits(responses, web, Default::default()).await
    }

    async fn new_for_limits(
        responses: Vec<String>,
        web: bool,
        limits: peri_cloud::limits::RuntimeLimits,
    ) -> Self {
        let root = tempfile::tempdir().unwrap();
        let stop = CancellationToken::new();
        let clock = Arc::new(Clock(AtomicI64::new(1_900_000_000)));
        let script = Arc::new(Script {
            replies: Mutex::new(responses.into()),
            ..Default::default()
        });
        let model_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let model_address = model_listener.local_addr().unwrap();
        let app = Router::new()
            .route("/v1/chat/completions", post(model_request))
            .with_state(script.clone());
        let shutdown = stop.clone();
        let model_server = tokio::spawn(async move {
            axum::serve(model_listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .unwrap();
        });
        let model = Arc::new(OpenAiModel::new(OpenAiConfig::new(
            format!("http://{model_address}/v1").parse().unwrap(),
            "fixture-model-key",
            "fixture-model",
        )));
        let connector = NativeSessionConnector::new(model, "You are a personal cloud agent. Use only the selected device. Keep tool and reasoning events internal.".into(), 128_000).unwrap();
        let adapter = Arc::new(Adapter::default());
        let cloud = CloudSide::start_with_limits(
            root.path(),
            clock.clone(),
            connector.clone(),
            adapter.clone(),
            true,
            web,
            limits.clone(),
        )
        .await;
        let mut f = Self {
            root,
            limits,
            web,
            submissions: Vec::new(),
            hidden_receipts: Vec::new(),
            crash_config: json!({"model_url":format!("http://{model_address}/v1"),"clients":[]}),
            held_submissions: Vec::new(),
            submission_release: Vec::new(),
            cloud: Some(cloud),
            clock,
            connector,
            devices: Vec::new(),
            workspaces: Vec::new(),
            executors: Vec::new(),
            _executor_roots: Vec::new(),
            adapter,
            script,
            stop,
            servers: vec![model_server],
            route: ChannelRoute {
                identity: ChannelIdentity {
                    adapter_instance_id: "fixture-qq".into(),
                    external_user_id: "owner-openid".into(),
                },
                conversation_id: "c2c-owner".into(),
            },
        };
        for index in 0..2 {
            f.add_device(index).await;
        }
        f
    }

    fn c(&self) -> &CloudSide {
        self.cloud.as_ref().unwrap()
    }
    fn message(&self, event: &str, text: &str) -> InboundMessage {
        InboundMessage {
            route: self.route.clone(),
            event_id: event.into(),
            reply_kind: peri_cloud::gateway::ReplyKind::Message,
            text: text.into(),
        }
    }

    async fn add_device(&mut self, index: usize) {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let executor = Arc::new(
            Executor::open(root.path(), &format!("fixture-pc-{index}"))
                .await
                .unwrap(),
        );
        let token = std::fs::read_to_string(root.path().join("transport-token")).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client_address = listener.local_addr().unwrap();
        let shutdown = self.stop.clone();
        let submissions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hidden = Arc::new(AtomicBool::new(false));
        let held = Arc::new(AtomicBool::new(false));
        let release = Arc::new(Notify::new());
        self.held_submissions.push(held.clone());
        self.submission_release.push(release.clone());
        self.submissions.push(submissions.clone());
        self.hidden_receipts.push(hidden.clone());
        let app = web::router(executor.clone()).layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let submissions = submissions.clone();
                let hidden = hidden.clone();
                let held = held.clone();
                let release = release.clone();
                async move {
                    let submission = request.method() == axum::http::Method::POST
                        && request.uri().path().ends_with("/jobs");
                    if request.method() == axum::http::Method::POST
                        && request.uri().path().ends_with("/jobs")
                    {
                        submissions.fetch_add(1, Ordering::SeqCst);
                    }
                    if request.method() == axum::http::Method::GET
                        && request.uri().path().contains("/jobs/")
                        && hidden.load(Ordering::SeqCst)
                    {
                        return axum::response::IntoResponse::into_response(
                            axum::http::StatusCode::NOT_FOUND,
                        );
                    }
                    let response = next.run(request).await;
                    if submission && held.load(Ordering::SeqCst) {
                        release.notified().await;
                    }
                    response
                }
            },
        ));
        self.servers.push(tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .unwrap();
        }));
        self.crash_config["clients"].as_array_mut().unwrap().push(json!({"address":client_address.to_string(),"token":token,"device":executor.info().device_id}));
        let client = DeviceClient::connect(client_address, token, executor.info().device_id)
            .await
            .unwrap();
        self.connector.register_client(client);
        let authorization = NativeAuthorization {
            client_id: "peri-executor".into(),
            device_id: executor.info().device_id,
            device_name: format!("PC {index}"),
            redirect_uri: "http://127.0.0.1:45678/oauth/callback".into(),
            code_challenge: URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes())),
            code_challenge_method: "S256".into(),
            state: "fixture-native-state".into(),
        };
        let redirect = self
            .c()
            .identity
            .authorize_native(&self.c().browser, authorization.clone())
            .await
            .unwrap();
        let code = url::Url::parse(&redirect)
            .unwrap()
            .query_pairs()
            .find(|(key, _)| key == "code")
            .unwrap()
            .1
            .into_owned();
        let tokens = self
            .c()
            .identity
            .exchange_code(
                "peri-executor",
                &code,
                VERIFIER,
                &authorization.redirect_uri,
            )
            .await
            .unwrap();
        let actor = self
            .c()
            .identity
            .authenticate_native(&tokens.access_token)
            .await
            .unwrap();
        let device = DeviceRecord {
            id: executor.info().device_id,
            name: format!("PC {index}"),
            platform: executor.info().platform.clone(),
            default_workspace: workspace.path().to_str().unwrap().into(),
            connection_id: None,
            is_local: false,
            revoked: false,
        };
        self.c()
            .identity
            .register_device(&actor, device.clone())
            .await
            .unwrap();
        self.devices.push(device);
        self.workspaces.push(workspace);
        self.executors.push(executor);
        self._executor_roots.push(root);
    }

    async fn pair(&self) {
        let code = self
            .c()
            .identity
            .create_pair_code(&self.c().browser)
            .await
            .unwrap();
        let message = self.message("pair-event", &format!("/绑定 {}", code.code));
        let receipt = self.c().gateway.receive(message.clone()).await.unwrap();
        assert_eq!(receipt.state, ReceiptState::Completed);
        assert!(
            matches!(
                self.c()
                    .identity
                    .resolve_channel(&self.route.identity)
                    .await,
                Err(IdentityError::Unauthorized)
            ),
            "电脑确认前不可关联"
        );
        self.c().gateway.receive(message).await.unwrap();
        let claims = self
            .c()
            .identity
            .pending_pair_claims(&self.c().browser)
            .await
            .unwrap();
        assert_eq!(claims.len(), 1, "重复消息不重复消耗口令");
        self.c()
            .identity
            .confirm_pair_claim(&self.c().browser, claims[0].claim_id, true)
            .await
            .unwrap();
        self.c().gateway.dispatch().await.unwrap();
        self.adapter.attempts.lock().clear();
    }

    async fn select(&self, event: &str, index: usize) {
        self.c()
            .gateway
            .receive(self.message(event, &format!("/连接 {}", self.devices[index].id)))
            .await
            .unwrap();
        self.c().gateway.dispatch().await.unwrap();
    }

    async fn card(&self) -> InteractionCard {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                self.c().gateway.dispatch().await.unwrap();
                if let Some(card) = self
                    .c()
                    .gateway
                    .browser_interactions(&self.c().browser)
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|c| {
                        matches!(
                            c.context,
                            peri_acp_types::interaction::InteractionContext::Questions { .. }
                        )
                    })
                {
                    break card;
                }
                if let Some(card) =
                    self.adapter
                        .attempts
                        .lock()
                        .iter()
                        .find_map(|delivery| match &delivery.body {
                            DeliveryBody::Interaction { card } => Some(card.clone()),
                            _ => None,
                        })
                {
                    break card;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("必须收到真实 Peri 审批")
    }

    async fn settled(&self, session: Uuid, turn: Uuid) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let record = self
                    .c()
                    .journal
                    .turn(self.c().browser.principal().id, session, turn)
                    .await
                    .unwrap();
                if !matches!(record.state, TurnState::Queued | TurnState::Running) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("真实云循环必须结算");
    }

    async fn restart(&mut self) {
        let old = self.cloud.take().unwrap();
        old.stop().await;
        drop(old);
        self.cloud = Some(
            CloudSide::start_with_limits(
                self.root.path(),
                self.clock.clone(),
                self.connector.clone(),
                self.adapter.clone(),
                false,
                self.web,
                self.limits.clone(),
            )
            .await,
        );
    }

    async fn finish(mut self) {
        self.cloud.take().unwrap().stop().await;
        for executor in &self.executors {
            executor.shutdown().await.expect("执行器须停止实际任务");
        }
        self.stop.cancel();
        for server in self.servers.drain(..) {
            server.await.unwrap();
        }
    }
}

#[path = "gateway/lifecycle.rs"]
mod lifecycle_tests;
