use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use ed25519_dalek::{Signer, SigningKey};
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use peri_cloud::gateway::{Gateway, GatewayError, MessageAdapter, SessionConnector};
use peri_cloud::identity::{DeviceRecord, IdentityService};
use peri_cloud::qq::{webhook_router, QqAdapter, QqEndpoints, QqInteractionMode};
use peri_cloud::state::{CloudJournal, FrozenSession};
use peri_cloud::{CloudAgent, CloudRuntime};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const SECRET: &str = "fixture-qq-secret-32-byte-padding";

struct NoDevices;
#[async_trait]
impl SessionConnector for NoDevices {
    async fn open(
        &self,
        _principal: Uuid,
        _device: &DeviceRecord,
        _previous: Option<&FrozenSession>,
    ) -> Result<Arc<CloudAgent>, GatewayError> {
        Err(GatewayError::Connection)
    }
}

struct Api {
    gateway_url: String,
    messages: Mutex<Vec<Value>>,
}
async fn outgoing(State(api): State<Arc<Api>>, Json(body): Json<Value>) -> Json<Value> {
    api.messages.lock().push(body);
    Json(json!({"id":"outgoing-id"}))
}

struct Fixture {
    _root: tempfile::TempDir,
    gateway: Arc<Gateway>,
    runtime: Arc<CloudRuntime>,
    adapter: Arc<QqAdapter>,
    api: Arc<Api>,
    stop: CancellationToken,
    api_server: tokio::task::JoinHandle<()>,
}
impl Fixture {
    async fn new(gateway_url: String) -> Self {
        let api = Arc::new(Api {
            gateway_url,
            messages: Mutex::new(Vec::new()),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}/", listener.local_addr().unwrap());
        let routes =
            Router::new()
                .route(
                    "/token",
                    post(|| async {
                        Json(json!({"access_token":"fixture-token","expires_in":3600}))
                    }),
                )
                .route(
                    "/gateway",
                    get(|State(api): State<Arc<Api>>| async move {
                        Json(json!({"url":api.gateway_url}))
                    }),
                )
                .route("/v2/users/{user}/messages", post(outgoing))
                .with_state(api.clone());
        let stop = CancellationToken::new();
        let signal = stop.clone();
        let api_server = tokio::spawn(async move {
            axum::serve(listener, routes)
                .with_graceful_shutdown(signal.cancelled_owned())
                .await
                .unwrap();
        });
        let adapter = QqAdapter::new(
            "fixture-qq".into(),
            "123".into(),
            SECRET.into(),
            "https://agent.example.test",
            QqInteractionMode::default(),
            QqEndpoints::loopback(
                origin.parse().unwrap(),
                format!("{origin}token").parse().unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let journal = Arc::new(CloudJournal::open(root.path()).await.unwrap());
        let identity =
            IdentityService::new(&journal, "fixture-setup-secret-at-least-32-characters")
                .await
                .unwrap();
        let runtime = CloudRuntime::new(journal);
        let gateway = Gateway::new(
            runtime.clone(),
            identity,
            Arc::new(NoDevices),
            vec![adapter.clone() as Arc<dyn MessageAdapter>],
            8,
        )
        .await
        .unwrap();
        Self {
            _root: root,
            gateway,
            runtime,
            adapter,
            api,
            stop,
            api_server,
        }
    }
    async fn finish(self) {
        assert!(self.gateway.shutdown(Duration::from_secs(3)).await.unwrap());
        assert!(self.runtime.shutdown(Duration::from_secs(3)).await.complete);
        self.stop.cancel();
        self.api_server.await.unwrap();
    }
}

fn dispatch(sequence: u64) -> Value {
    json!({"op":0,"s":sequence,"t":"C2C_MESSAGE_CREATE","d":{"id":"message-once","content":"/帮助","author":{"user_openid":"owner-openid"}}})
}

async fn heartbeat_until(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    sequence: u64,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let message = socket.next().await.unwrap().unwrap();
            let value: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
            assert_eq!(value["op"], 1);
            socket
                .send(Message::Text(json!({"op":11}).to_string().into()))
                .await
                .unwrap();
            if value["d"] == json!(sequence) {
                break;
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn websocket_identifies_with_buttons_resumes_processed_sequence_and_deduplicates_message() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let f = Fixture::new(format!("ws://{}", listener.local_addr().unwrap())).await;
    let (proof, complete) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        for attempt in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket
                .send(Message::Text(
                    json!({"op":10,"d":{"heartbeat_interval":1000}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            let message = socket.next().await.unwrap().unwrap();
            let identify: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
            assert_eq!(identify["d"]["token"], "QQBot fixture-token");
            if attempt == 0 {
                assert_eq!(identify["op"], 2);
                assert_eq!(identify["d"]["intents"], (1u64 << 25) | (1u64 << 26));
                socket
                    .send(Message::Text(
                        json!({"op":0,"s":1,"t":"READY","d":{"session_id":"fixture-session"}})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                socket
                    .send(Message::Text(dispatch(2).to_string().into()))
                    .await
                    .unwrap();
                heartbeat_until(&mut socket, 2).await;
                socket.close(None).await.unwrap();
            } else {
                assert_eq!(identify["op"], 6);
                assert_eq!(identify["d"]["session_id"], "fixture-session");
                assert_eq!(identify["d"]["seq"], 2);
                socket
                    .send(Message::Text(
                        json!({"op":0,"s":3,"t":"RESUMED","d":{}})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                socket
                    .send(Message::Text(dispatch(4).to_string().into()))
                    .await
                    .unwrap();
                heartbeat_until(&mut socket, 4).await;
            }
        }
        proof.send(()).unwrap();
    });
    let stop = CancellationToken::new();
    let client = tokio::spawn(
        f.adapter
            .clone()
            .run_websocket(f.gateway.clone(), stop.clone()),
    );
    tokio::time::timeout(Duration::from_secs(15), complete)
        .await
        .unwrap()
        .unwrap();
    stop.cancel();
    client.await.unwrap().unwrap();
    server.await.unwrap();
    f.gateway.dispatch().await.unwrap();
    let messages = f.api.messages.lock().clone();
    assert_eq!(messages.len(), 1, "重连补发不产生重复回复");
    assert!(messages[0]["content"].as_str().unwrap().contains("/绑定"));
    f.finish().await;
}

fn key() -> SigningKey {
    let mut seed = [0u8; 32];
    for (index, byte) in seed.iter_mut().enumerate() {
        *byte = SECRET.as_bytes()[index % SECRET.len()];
    }
    SigningKey::from_bytes(&seed)
}

/// [回归测试] READY 前的立即心跳会被腾讯丢弃，下一拍不能误报失联。
#[tokio::test]
async fn websocket_waits_for_authentication_before_heartbeats_and_keeps_acknowledged_connection() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let f = Fixture::new(format!("ws://{}", listener.local_addr().unwrap())).await;
    let (proof, complete) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket
            .send(Message::Text(
                json!({"op":10,"d":{"heartbeat_interval":1000}})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let identify: Value =
            serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(identify["op"], 2);
        // 模拟鉴权窗口：窗口内的心跳被丢弃，不 ACK。窗口结束才发 READY。
        let premature = match tokio::time::timeout(Duration::from_millis(100), socket.next()).await
        {
            Ok(Some(Ok(message))) => {
                let value: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
                assert_eq!(value["op"], 1);
                1
            }
            Err(_) => 0,
            _ => panic!("客户端在鉴权期间断开"),
        };
        socket
            .send(Message::Text(
                json!({"op":0,"s":1,"t":"READY","d":{"session_id":"fixture-delayed-auth"}})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let mut acknowledged = 0;
        for _ in 0..2 {
            let frame = tokio::time::timeout(Duration::from_secs(3), socket.next()).await;
            let Ok(Some(Ok(Message::Text(text)))) = frame else {
                break;
            };
            let value: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(value["op"], 1);
            assert_eq!(value["d"], 1, "心跳携带已处理的 READY 序号");
            socket
                .send(Message::Text(json!({"op":11}).to_string().into()))
                .await
                .unwrap();
            acknowledged += 1;
        }
        proof.send((premature, acknowledged)).unwrap();
        let _ = released.await;
    });
    let stop = CancellationToken::new();
    let client = tokio::spawn(
        f.adapter
            .clone()
            .run_websocket(f.gateway.clone(), stop.clone()),
    );
    let facts = tokio::time::timeout(Duration::from_secs(8), complete)
        .await
        .unwrap()
        .unwrap();
    stop.cancel();
    client.await.unwrap().unwrap();
    let _ = release.send(());
    server.await.unwrap();
    f.finish().await;
    assert_eq!(facts, (0, 2), "鉴权前不发心跳，连续两次 ACK 不误触发重连");
}
fn signature(timestamp: &str, body: &str) -> String {
    key()
        .sign(format!("{timestamp}{body}").as_bytes())
        .to_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[tokio::test]
async fn webhook_checks_raw_signature_app_id_and_duplicates_before_admitting_gateway() {
    let f = Fixture::new("ws://127.0.0.1:1".into()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/qq/events", listener.local_addr().unwrap());
    let routes = webhook_router(f.adapter.clone(), f.gateway.clone());
    let signal = f.stop.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, routes)
            .with_graceful_shutdown(signal.cancelled_owned())
            .await
            .unwrap();
    });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let challenge = client
        .post(&url)
        .header("X-Bot-Appid", "123")
        .json(&json!({"op":13,"d":{"plain_token":"fixture-challenge","event_ts":"1725442341"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(challenge.status(), reqwest::StatusCode::OK);
    let challenge: Value = challenge.json().await.unwrap();
    assert_eq!(
        challenge["signature"],
        signature("1725442341", "fixture-challenge")
    );
    let forged_challenge = client
        .post(&url)
        .header("X-Bot-Appid", "123")
        .json(&json!({"op":13,"d":{"plain_token":dispatch(0).to_string(),"event_ts":"1725442341"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(forged_challenge.status(), reqwest::StatusCode::BAD_REQUEST);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    let body = dispatch(1).to_string();
    let signed = signature(&timestamp, &body);
    for (appid, sig, raw) in [
        ("wrong", signed.as_str(), body.clone()),
        ("123", "invalid", body.clone()),
        ("123", signed.as_str(), format!("{body} ")),
    ] {
        let response = client
            .post(&url)
            .header("X-Bot-Appid", appid)
            .header("X-Signature-Timestamp", &timestamp)
            .header("X-Signature-Ed25519", sig)
            .body(raw)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    }
    for _ in 0..2 {
        let response = client
            .post(&url)
            .header("X-Bot-Appid", "123")
            .header("X-Signature-Timestamp", &timestamp)
            .header("X-Signature-Ed25519", &signed)
            .body(body.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.json::<Value>().await.unwrap(), json!({"op":12}));
    }
    f.gateway.dispatch().await.unwrap();
    assert_eq!(f.api.messages.lock().len(), 1);
    f.finish().await;
    server.await.unwrap();
}
