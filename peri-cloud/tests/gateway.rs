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
}

async fn model_request(
    State(script): State<Arc<Script>>,
    Json(request): Json<Value>,
) -> ([(axum::http::HeaderName, &'static str); 1], String) {
    script.requests.lock().push(request);
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
        connector: Arc<NativeSessionConnector>,
        adapter: Arc<Adapter>,
        initialize: bool,
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
        let runtime = CloudRuntime::new(journal.clone());
        let gateway = Gateway::new(
            runtime.clone(),
            identity.clone(),
            connector,
            vec![adapter],
            8,
        )
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
}

impl Fixture {
    async fn new(responses: Vec<String>) -> Self {
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
        let cloud = CloudSide::start(
            root.path(),
            clock.clone(),
            connector.clone(),
            adapter.clone(),
            true,
        )
        .await;
        let mut f = Self {
            root,
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
        let app = web::router(executor.clone());
        self.servers.push(tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .unwrap();
        }));
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
            CloudSide::start(
                self.root.path(),
                self.clock.clone(),
                self.connector.clone(),
                self.adapter.clone(),
                false,
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

#[tokio::test]
async fn gateway_pc_confirm_select_approve_once_real_write_and_only_ai_reply() {
    let f = Fixture::new(vec![tool_write(), answer("文件已写入。")]).await;
    f.pair().await;
    f.select("select-pc", 0).await;
    f.adapter.attempts.lock().clear();
    let message = f.message("chat-write", "在当前电脑创建文件");
    let receipt = f.c().gateway.receive(message.clone()).await.unwrap();
    assert_eq!(receipt.state, ReceiptState::Submitted);
    let card = f.card().await;
    assert_eq!(card.turn_id, receipt.turn_id.unwrap());
    assert_eq!(card.device_id, f.devices[0].id);
    assert!(
        !f.workspaces[0].path().join("agent.txt").exists(),
        "批准前不得写入"
    );
    let mut wrong_route = f.route.clone();
    wrong_route.conversation_id = "another-chat".into();
    assert!(matches!(
        f.c()
            .gateway
            .respond(
                &wrong_route,
                card.request_id,
                InteractionAction::AllowOnce {}
            )
            .await,
        Err(GatewayError::StaleInteraction)
    ));
    f.c()
        .gateway
        .respond(&f.route, card.request_id, InteractionAction::AllowOnce {})
        .await
        .unwrap();
    assert!(
        matches!(
            f.c()
                .gateway
                .respond(&f.route, card.request_id, InteractionAction::AllowOnce {})
                .await,
            Err(GatewayError::StaleInteraction)
        ),
        "批准不能复用"
    );
    f.settled(card.session_id, card.turn_id).await;
    f.c().gateway.dispatch().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(f.workspaces[0].path().join("agent.txt")).unwrap(),
        "PRIVATE_TOOL_CONTENT"
    );
    assert!(
        !f.workspaces[1].path().join("agent.txt").exists(),
        "不能写到另一电脑"
    );
    let replies: Vec<_> = f
        .adapter
        .attempts
        .lock()
        .iter()
        .filter_map(|delivery| match &delivery.body {
            DeliveryBody::Reply { reply } => Some(reply.text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(replies, vec!["文件已写入。"], "普通聊天只包含 AI 文本");
    let duplicate = f.c().gateway.receive(message.clone()).await.unwrap();
    assert_eq!(duplicate.turn_id, receipt.turn_id);
    f.c().gateway.dispatch().await.unwrap();
    assert_eq!(
        f.script.requests.lock().len(),
        2,
        "平台重投不再调用模型或执行工具"
    );
    let mut conflicting = message;
    conflicting.text = "用同一平台消息ID换任务".into();
    assert!(matches!(
        f.c().gateway.receive(conflicting).await,
        Err(GatewayError::MessageConflict)
    ));
    let serialized = serde_json::to_value(f.adapter.attempts.lock().last().unwrap()).unwrap();
    assert!(serialized.get("internal_events").is_none());
    assert!(!serialized.to_string().contains("PRIVATE_REASONING"));
    f.finish().await;
}

#[tokio::test]
async fn gateway_busy_controls_and_cancel_reject_stale_approval() {
    let f = Fixture::new(vec![tool_write()]).await;
    f.pair().await;
    f.select("select-pc", 0).await;
    let receipt = f
        .c()
        .gateway
        .receive(f.message("chat-write", "创建文件"))
        .await
        .unwrap();
    let card = f.card().await;
    f.c()
        .gateway
        .receive(f.message("switch-busy", &format!("/连接 {}", f.devices[1].id)))
        .await
        .unwrap();
    f.c()
        .gateway
        .receive(f.message("mode-busy", "/权限 全部"))
        .await
        .unwrap();
    let before = f
        .c()
        .journal
        .session(f.c().browser.principal().id, card.session_id)
        .await
        .unwrap();
    assert_eq!(before.permission_mode, 0, "活动审批不改变权限快照");
    f.c()
        .gateway
        .receive(f.message("cancel", "/取消"))
        .await
        .unwrap();
    assert!(matches!(
        f.c()
            .gateway
            .respond(&f.route, card.request_id, InteractionAction::AllowOnce {})
            .await,
        Err(GatewayError::StaleInteraction)
    ));
    f.settled(card.session_id, receipt.turn_id.unwrap()).await;
    assert!(!f.workspaces[0].path().join("agent.txt").exists());
    assert!(!f.workspaces[1].path().join("agent.txt").exists());
    assert_eq!(f.script.requests.lock().len(), 1);
    f.finish().await;
}

#[tokio::test]
async fn gateway_device_revoke_invalidates_pending_approval() {
    let f = Fixture::new(vec![tool_write()]).await;
    f.pair().await;
    f.select("select-pc", 0).await;
    f.c()
        .gateway
        .receive(f.message("chat-write", "创建文件"))
        .await
        .unwrap();
    let card = f.card().await;
    f.c()
        .identity
        .revoke_device(&f.c().browser, card.device_id)
        .await
        .unwrap();
    assert!(matches!(
        f.c()
            .gateway
            .respond(&f.route, card.request_id, InteractionAction::AllowOnce {})
            .await,
        Err(GatewayError::Identity(IdentityError::Forbidden))
    ));
    f.c()
        .runtime
        .cancel(f.c().browser.principal().id, card.session_id, card.turn_id)
        .await
        .unwrap();
    f.settled(card.session_id, card.turn_id).await;
    assert!(!f.workspaces[0].path().join("agent.txt").exists());
    f.finish().await;
}

#[tokio::test]
async fn gateway_cloud_restart_delivers_saved_ai_reply_and_target_switch_resets_policy() {
    let mut f = Fixture::new(vec![
        answer("第一台电脑完成。"),
        answer("继续使用第一台电脑。"),
        tool_write(),
        answer("第二台电脑完成。"),
    ])
    .await;
    f.pair().await;
    f.select("select-first", 0).await;
    f.c()
        .gateway
        .receive(f.message("mode", "/权限 全部"))
        .await
        .unwrap();
    let receipt = f
        .c()
        .gateway
        .receive(f.message("first-chat", "回答当前任务"))
        .await
        .unwrap();
    let turn = receipt.turn_id.unwrap();
    let session = receipt.session_id.unwrap();
    f.settled(session, turn).await;
    f.adapter.attempts.lock().clear();
    f.restart().await;
    f.c().gateway.dispatch().await.unwrap();
    let replies: Vec<_> = f
        .adapter
        .attempts
        .lock()
        .iter()
        .filter_map(|delivery| match &delivery.body {
            DeliveryBody::Reply { reply } => Some(reply.text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        replies,
        vec!["第一台电脑完成。"],
        "新宿主投递已结算回复，不重新推理"
    );
    assert_eq!(f.script.requests.lock().len(), 1);
    let duplicate = f
        .c()
        .gateway
        .receive(f.message("first-chat", "回答当前任务"))
        .await
        .unwrap();
    assert_eq!(duplicate.turn_id, Some(turn));
    let resumed = f
        .c()
        .gateway
        .receive(f.message("resumed-chat", "继续上一个会话"))
        .await
        .unwrap();
    assert_eq!(resumed.session_id, Some(session), "重开宿主应重接原会话");
    f.settled(session, resumed.turn_id.unwrap()).await;
    assert_eq!(
        f.c()
            .journal
            .session(f.c().browser.principal().id, session)
            .await
            .unwrap()
            .permission_mode,
        4
    );
    let history = serde_json::to_string(&f.script.requests.lock()[1]).unwrap();
    assert!(
        history.contains("第一台电脑完成。"),
        "重开宿主应恢复 canonical 聊天历史"
    );
    f.select("select-second", 1).await;
    f.adapter.attempts.lock().clear();
    f.c()
        .gateway
        .receive(f.message("second-chat", "创建文件"))
        .await
        .unwrap();
    let card = f.card().await;
    assert_ne!(card.session_id, session);
    assert_eq!(card.device_id, f.devices[1].id);
    assert_eq!(
        f.c()
            .journal
            .session(f.c().browser.principal().id, card.session_id)
            .await
            .unwrap()
            .permission_mode,
        0,
        "新电脑不继承全部允许"
    );
    f.c()
        .gateway
        .respond(&f.route, card.request_id, InteractionAction::AllowOnce {})
        .await
        .unwrap();
    f.settled(card.session_id, card.turn_id).await;
    assert!(!f.workspaces[0].path().join("agent.txt").exists());
    assert_eq!(
        std::fs::read_to_string(f.workspaces[1].path().join("agent.txt")).unwrap(),
        "PRIVATE_TOOL_CONTENT"
    );
    f.finish().await;
}

#[tokio::test]
async fn gateway_unknown_delivery_is_not_replayed_after_restart() {
    let mut f = Fixture::new(vec![]).await;
    f.adapter.fail_next.store(2, Ordering::SeqCst);
    f.c()
        .gateway
        .receive(f.message("help", "/帮助"))
        .await
        .unwrap();
    f.c().gateway.dispatch().await.unwrap();
    let first = f.adapter.attempts.lock()[0].clone();
    assert_eq!(first.message_sequence, 1);
    f.restart().await;
    f.c()
        .gateway
        .receive(f.message("help", "/帮助"))
        .await
        .unwrap();
    f.c().gateway.dispatch().await.unwrap();
    assert_eq!(
        f.adapter.attempts.lock().len(),
        1,
        "网络结果未知不能重复发送"
    );
    let encoded = serde_json::to_string(&first).unwrap();
    let decoded: Delivery = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded.delivery_id, first.delivery_id);
    assert_eq!(decoded.route, first.route);
    assert!(serde_json::from_value::<InboundMessage>(json!({"event_id":"missing-route"})).is_err());
    assert!(
        serde_json::from_value::<InteractionAction>(
            json!({"kind":"allow_once","allow_always":true})
        )
        .is_err(),
        "按钮协议不接受额外权限参数"
    );
    f.finish().await;
}

#[tokio::test]
async fn gateway_disconnected_dispatch_waiter_does_not_abort_owned_delivery() {
    let f = Fixture::new(vec![]).await;
    f.c()
        .gateway
        .receive(f.message("help", "/帮助"))
        .await
        .unwrap();
    f.adapter.block_next.store(true, Ordering::SeqCst);
    let gateway = f.c().gateway.clone();
    let waiter = tokio::spawn(async move { gateway.dispatch().await });
    tokio::time::timeout(Duration::from_secs(5), f.adapter.blocked.notified())
        .await
        .unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    assert!(
        !f.c().gateway.shutdown(Duration::ZERO).await.unwrap(),
        "发送仍在运行，不能假报关闭"
    );
    f.adapter.release.notify_one();
    assert!(f
        .c()
        .gateway
        .shutdown(Duration::from_secs(3))
        .await
        .unwrap());
    assert_eq!(f.adapter.attempts.lock().len(), 1);
    f.finish().await;
}

#[tokio::test]
async fn gateway_expired_card_and_revoked_channel_cannot_approve() {
    let f = Fixture::new(vec![tool_write()]).await;
    f.pair().await;
    f.select("select-pc", 0).await;
    f.c()
        .gateway
        .receive(f.message("chat-write", "创建文件"))
        .await
        .unwrap();
    let card = f.card().await;
    f.clock.0.fetch_add(301, Ordering::SeqCst);
    assert!(
        matches!(
            f.c()
                .gateway
                .respond(&f.route, card.request_id, InteractionAction::AllowOnce {})
                .await,
            Err(GatewayError::StaleInteraction)
        ),
        "有效期到达后拒绝授权"
    );
    f.c()
        .identity
        .revoke_channel(&f.c().browser, &f.route.identity)
        .await
        .unwrap();
    assert!(
        matches!(
            f.c()
                .gateway
                .receive(f.message("chat-write", "创建文件"))
                .await,
            Err(GatewayError::Identity(IdentityError::Unauthorized))
        ),
        "重投消息也不能绕过渠道撤销"
    );
    f.c()
        .gateway
        .receive(f.message("after-revoke", "/权限 全部"))
        .await
        .unwrap();
    assert_eq!(
        f.c()
            .journal
            .session(f.c().browser.principal().id, card.session_id)
            .await
            .unwrap()
            .permission_mode,
        0,
        "撤销渠道后不能修改权限"
    );
    f.c()
        .runtime
        .cancel(f.c().browser.principal().id, card.session_id, card.turn_id)
        .await
        .unwrap();
    f.settled(card.session_id, card.turn_id).await;
    assert!(!f.workspaces[0].path().join("agent.txt").exists());
    assert_eq!(f.script.requests.lock().len(), 1);
    f.finish().await;
}

#[tokio::test]
async fn gateway_second_router_cannot_recover_a_live_owner() {
    let f = Fixture::new(vec![]).await;
    let second = Gateway::new(
        f.c().runtime.clone(),
        f.c().identity.clone(),
        f.connector.clone(),
        vec![f.adapter.clone()],
        8,
    )
    .await;
    assert!(
        matches!(second, Err(GatewayError::Busy)),
        "第二个路由器不能恢复仍在运行的消息发送"
    );
    f.finish().await;
}

#[tokio::test]
async fn gateway_delivery_panic_keeps_health_failed_and_restart_preserves_unknown_send() {
    let mut f = Fixture::new(vec![]).await;
    f.c()
        .gateway
        .receive(f.message("help", "/帮助"))
        .await
        .unwrap();
    f.adapter.panic_next.store(true, Ordering::SeqCst);
    assert!(matches!(
        f.c().gateway.dispatch().await,
        Err(GatewayError::RecoveryRequired)
    ));
    assert!(
        matches!(
            f.c().gateway.shutdown(Duration::from_secs(3)).await,
            Err(GatewayError::RecoveryRequired)
        ),
        "panic 必须持续报告未确认状态"
    );
    assert!(
        matches!(
            f.c()
                .gateway
                .receive(f.message("blocked-after-panic", "/帮助"))
                .await,
            Err(GatewayError::RecoveryRequired)
        ),
        "下一次请求不能清除未确认状态"
    );
    let old = f.cloud.take().unwrap();
    assert!(
        old.runtime.shutdown(Duration::from_secs(5)).await.complete,
        "已完成的渠道句柄不阻止云运行退出"
    );
    drop(old);
    f.cloud = Some(
        CloudSide::start(
            f.root.path(),
            f.clock.clone(),
            f.connector.clone(),
            f.adapter.clone(),
            false,
        )
        .await,
    );
    f.c().gateway.dispatch().await.unwrap();
    assert_eq!(
        f.adapter.attempts.lock().len(),
        1,
        "sending 重开后保留结果未知，不重新发送"
    );
    f.c()
        .gateway
        .receive(f.message("new-help", "/帮助"))
        .await
        .unwrap();
    f.c().gateway.dispatch().await.unwrap();
    assert_eq!(f.adapter.attempts.lock().len(), 2, "恢复后允许新的独立消息");
    f.finish().await;
}
