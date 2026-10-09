use std::sync::atomic::{AtomicUsize, Ordering};
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
    ChannelRoute, Delivery, DeliveryError, Gateway, InboundMessage, MessageAdapter,
    NativeSessionConnector,
};
use peri_cloud::identity::{ChannelIdentity, DeviceRecord, IdentityService, NativeAuthorization};
use peri_cloud::portal::{self, PortalConfig};
use peri_cloud::state::CloudJournal;
use peri_cloud::CloudRuntime;
use peri_executor::{web, Executor};
use peri_model::{OpenAiConfig, OpenAiModel};
use peri_remote_tools::DeviceClient;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

const SETUP: &str = "fixture-browser-approval-bootstrap-at-least-32-bytes";
const PASSWORD: &str = "fixture-browser-approval-password";
const VERIFIER: &str = "fixture-browser-native-verifier-at-least-43-characters";

#[derive(Default)]
struct Adapter(Mutex<Vec<Delivery>>);
#[async_trait]
impl MessageAdapter for Adapter {
    fn instance_id(&self) -> &str {
        "fixture-qq"
    }
    async fn deliver(&self, delivery: &Delivery) -> Result<(), DeliveryError> {
        self.0.lock().push(delivery.clone());
        Ok(())
    }
}

async fn model(
    State(calls): State<Arc<AtomicUsize>>,
    Json(_body): Json<Value>,
) -> ([(axum::http::HeaderName, &'static str); 1], String) {
    let delta = match calls.fetch_add(1, Ordering::SeqCst) {
        0 => {
            json!({"choices":[{"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"browser-write","type":"function","function":{"name":"Write","arguments":json!({"file_path":"approved.txt","content":"PRIVATE_TOOL_CONTENT"}).to_string()}}]},"finish_reason":"tool_calls"}]})
        }
        1 => {
            json!({"choices":[{"delta":{"role":"assistant","content":"文件已写入。","reasoning_content":"PRIVATE_REASONING"},"finish_reason":"stop"}]})
        }
        _ => panic!("unexpected model call"),
    };
    (
        [(CONTENT_TYPE, "text/event-stream")],
        format!("data: {delta}\n\ndata: [DONE]\n\n"),
    )
}

#[tokio::test]
async fn browser_cookie_and_csrf_approve_the_existing_channel_request_once() {
    let root = tempfile::tempdir().unwrap();
    let executor_root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let stop = CancellationToken::new();
    let mut servers = Vec::new();
    let journal = Arc::new(CloudJournal::open(root.path()).await.unwrap());
    let identity = IdentityService::new(&journal, SETUP).await.unwrap();
    identity
        .initialize(SETUP, "owner", PASSWORD.into(), "Owner")
        .await
        .unwrap();
    let login = identity.login("owner", PASSWORD.into()).await.unwrap();
    let actor = identity
        .authenticate_browser(&login.session_token)
        .await
        .unwrap();
    let executor = Arc::new(Executor::open(executor_root.path(), "PC").await.unwrap());
    let token = std::fs::read_to_string(executor_root.path().join("transport-token")).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let signal = stop.clone();
    let app = web::router(executor.clone());
    servers.push(tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(signal.cancelled_owned())
            .await
            .unwrap();
    }));
    let device_client = DeviceClient::connect(address, token, executor.info().device_id)
        .await
        .unwrap();
    let authorization = NativeAuthorization {
        client_id: "peri-executor".into(),
        device_id: executor.info().device_id,
        device_name: "PC".into(),
        redirect_uri: "http://127.0.0.1:45678/oauth/callback".into(),
        code_challenge: URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes())),
        code_challenge_method: "S256".into(),
        state: "fixture-native-state".into(),
    };
    let redirect = identity
        .authorize_native(&actor, authorization.clone())
        .await
        .unwrap();
    let code = url::Url::parse(&redirect)
        .unwrap()
        .query_pairs()
        .find(|(key, _)| key == "code")
        .unwrap()
        .1
        .into_owned();
    let tokens = identity
        .exchange_code(
            "peri-executor",
            &code,
            VERIFIER,
            &authorization.redirect_uri,
        )
        .await
        .unwrap();
    let native = identity
        .authenticate_native(&tokens.access_token)
        .await
        .unwrap();
    identity
        .register_device(
            &native,
            DeviceRecord {
                id: executor.info().device_id,
                name: "PC".into(),
                platform: executor.info().platform.clone(),
                default_workspace: workspace.path().to_str().unwrap().into(),
                connection_id: None,
                revoked: false,
            },
        )
        .await
        .unwrap();
    let channel = ChannelIdentity {
        adapter_instance_id: "fixture-qq".into(),
        external_user_id: "owner-openid".into(),
    };
    let pair = identity.create_pair_code(&actor).await.unwrap();
    let claim = identity
        .claim_pair_code(channel.clone(), &pair.code)
        .await
        .unwrap();
    identity
        .confirm_pair_claim(&actor, claim.claim_id, true)
        .await
        .unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/v1/chat/completions", post(model))
        .with_state(calls.clone());
    let signal = stop.clone();
    servers.push(tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(signal.cancelled_owned())
            .await
            .unwrap();
    }));
    let model = Arc::new(OpenAiModel::new(OpenAiConfig::new(
        format!("http://{address}/v1").parse().unwrap(),
        "fixture-model-key",
        "fixture-model",
    )));
    let connector =
        NativeSessionConnector::new(model, "Work on the selected device.".into(), 128000).unwrap();
    connector.register_client(device_client);
    let runtime = CloudRuntime::new(journal.clone());
    let adapter = Arc::new(Adapter::default());
    let gateway = Gateway::new(
        runtime.clone(),
        identity.clone(),
        connector,
        vec![adapter.clone()],
        8,
    )
    .await
    .unwrap();
    let route = ChannelRoute {
        identity: channel,
        conversation_id: "c2c:owner-openid".into(),
    };
    gateway
        .receive(InboundMessage {
            route: route.clone(),
            event_id: "select".into(),
            text: format!("/连接 {}", executor.info().device_id),
        })
        .await
        .unwrap();
    let receipt = gateway
        .receive(InboundMessage {
            route,
            event_id: "write".into(),
            text: "创建文件".into(),
        })
        .await
        .unwrap();
    let cards = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let cards = gateway.browser_interactions(&actor).await.unwrap();
            if !cards.is_empty() {
                break cards;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].turn_id, receipt.turn_id.unwrap());
    assert!(matches!(
        gateway.browser_interactions(&native).await,
        Err(peri_cloud::gateway::GatewayError::Identity(
            peri_cloud::identity::IdentityError::Forbidden
        ))
    ));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = portal::router_with_gateway(
        identity.clone(),
        PortalConfig::new(&origin).unwrap(),
        gateway.clone(),
    );
    let signal = stop.clone();
    servers.push(tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(signal.cancelled_owned())
            .await
            .unwrap();
    }));
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = format!("{origin}/api/interactions/{}/respond", cards[0].request_id);
    let cookie = format!("peri_session={}", login.session_token);
    let unauthenticated = client
        .get(format!("{origin}/api/interactions"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), reqwest::StatusCode::UNAUTHORIZED);
    let exposed: Value = client
        .get(format!("{origin}/api/interactions"))
        .header("Cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        exposed.to_string().contains("PRIVATE_TOOL_CONTENT"),
        "完整参数只向已认证账户展示"
    );
    for (body, csrf, status) in [
        (
            json!({"kind":"allow_once"}),
            "wrong",
            reqwest::StatusCode::UNAUTHORIZED,
        ),
        (
            json!({"kind":"allow_once","route":{"user":"forged"}}),
            login.csrf_token.as_str(),
            reqwest::StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let response = client
            .post(&url)
            .header("Origin", &origin)
            .header("Cookie", &cookie)
            .header("X-Peri-CSRF", csrf)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        assert!(!workspace.path().join("approved.txt").exists());
    }
    let response = client
        .post(&url)
        .header("Origin", &origin)
        .header("Cookie", &cookie)
        .header("X-Peri-CSRF", &login.csrf_token)
        .json(&json!({"kind":"allow_once"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let duplicate = client
        .post(&url)
        .header("Origin", &origin)
        .header("Cookie", &cookie)
        .header("X-Peri-CSRF", &login.csrf_token)
        .json(&json!({"kind":"allow_once"}))
        .send()
        .await
        .unwrap();
    assert_eq!(duplicate.status(), reqwest::StatusCode::CONFLICT);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let turn = journal
                .turn(actor.principal().id, cards[0].session_id, cards[0].turn_id)
                .await
                .unwrap();
            if !matches!(
                turn.state,
                peri_cloud::state::TurnState::Queued | peri_cloud::state::TurnState::Running
            ) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("approved.txt")).unwrap(),
        "PRIVATE_TOOL_CONTENT"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    gateway.dispatch().await.unwrap();
    let replies: Vec<_> = adapter
        .0
        .lock()
        .iter()
        .filter_map(|delivery| match &delivery.body {
            peri_cloud::gateway::DeliveryBody::Reply { reply } => Some(reply.text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(replies, vec!["文件已写入。"]);
    assert!(gateway.shutdown(Duration::from_secs(3)).await.unwrap());
    assert!(runtime.shutdown(Duration::from_secs(5)).await.complete);
    executor.shutdown().await.unwrap();
    stop.cancel();
    for server in servers {
        server.await.unwrap();
    }
}
