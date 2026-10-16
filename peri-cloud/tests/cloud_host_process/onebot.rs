//! A real host, original Agent and executor; only the external QQ protocol and
//! model endpoints are replaced. Official and personal adapters run together.
use super::*;
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

pub(super) const TOKEN: &str = "fixture-onebot-token-at-least-32-characters";
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Personal {
    socket: Socket,
    sent: HashMap<i64, Value>,
    next_id: i64,
}

fn request(
    address: &str,
    token: &str,
    self_id: &str,
    role: &str,
) -> tokio_tungstenite::tungstenite::http::Request<()> {
    let mut request = format!("ws://{address}/onebot/v11")
        .into_client_request()
        .unwrap();
    for (name, value) in [
        ("Authorization", format!("Bearer {token}")),
        ("X-Self-ID", self_id.into()),
        ("X-Client-Role", role.into()),
    ] {
        request.headers_mut().insert(
            tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    request
}

impl Personal {
    async fn connect(address: &str) -> Self {
        let (socket, _) = connect_async(request(address, TOKEN, "12345", "Universal"))
            .await
            .unwrap();
        Self {
            socket,
            sent: HashMap::new(),
            next_id: 1000,
        }
    }
    async fn event(&mut self, id: i64, user: i64, group: Option<i64>, message: Value) {
        let value = json!({"post_type":"message","self_id":12345,"message_type":if group.is_some(){"group"}else{"private"},
            "sub_type":if group.is_some(){"normal"}else{"friend"},"message_id":id,"user_id":user,"group_id":group,"message":message});
        self.socket
            .send(Message::Text(value.to_string().into()))
            .await
            .unwrap();
    }
    async fn text(&mut self, id: i64, text: &str) {
        self.event(
            id,
            67890,
            Some(11111),
            json!([{"type":"text","data":{"text":text}}]),
        )
        .await;
    }
    async fn call(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match self.socket.next().await.unwrap().unwrap() {
                    Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                    Message::Ping(bytes) => self.socket.send(Message::Pong(bytes)).await.unwrap(),
                    _ => panic!("unexpected socket closure"),
                }
            }
        })
        .await
        .expect("OneBot call missing")
    }
    async fn response(&mut self, call: &Value, status: &str, retcode: i64, data: Value) {
        let response = json!({"echo":call["echo"],"status":status,"retcode":retcode,"data":data});
        self.socket
            .send(Message::Text(response.to_string().into()))
            .await
            .unwrap();
    }
    async fn reply(&mut self, needle: &str) -> (i64, String) {
        loop {
            let call = self.call().await;
            if call["action"] == "get_msg" {
                let id = call["params"]["message_id"].as_i64().unwrap();
                let original = self.sent.get(&id).cloned().unwrap_or(Value::Null);
                self.response(&call, "ok", 0, original).await;
                continue;
            }
            assert_eq!(call["action"], "send_group_msg");
            assert_eq!(call["params"]["group_id"], 11111);
            let message = &call["params"]["message"];
            let text = message
                .as_array()
                .unwrap()
                .iter()
                .filter(|s| s["type"] == "text")
                .map(|s| s["data"]["text"].as_str().unwrap())
                .collect::<String>();
            self.next_id += 1;
            let id = self.next_id;
            self.sent.insert(id,json!({"message_id":id,"message_type":"group","sender":{"user_id":12345},"message":message}));
            self.response(&call, "ok", 0, json!({"message_id":id}))
                .await;
            if text.contains(needle) {
                return (id, text);
            }
        }
    }
}

async fn confirm(process: &Process, client: &Client, account: &Account) {
    let data = process.get(client, account, "/api/account").await;
    let claims = data["pending_pair_claims"].as_array().unwrap();
    assert_eq!(claims.len(), 1);
    process
        .post(
            client,
            account,
            &format!(
                "/api/pairing/{}/confirm",
                claims[0]["claim_id"].as_str().unwrap()
            ),
            json!({"allow":true}),
        )
        .await;
}

struct Fixture {
    _root: tempfile::TempDir,
    path: std::path::PathBuf,
    base: String,
    address: String,
    workspace: std::path::PathBuf,
    process: Process,
    client: Client,
    account: Account,
    wire: Arc<Wire>,
    stop: CancellationToken,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("personal workspace");
        let wire = Arc::new(Wire::default());
        let stop = CancellationToken::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route(
                "/token",
                post(|| async { Json(json!({"access_token":"fixture-token","expires_in":3600})) }),
            )
            .route("/v2/users/{user}/messages", post(outgoing))
            .route("/interactions/{id}", put(acknowledge))
            .route("/v1/chat/completions", post(model))
            .with_state(wire.clone());
        let signal = stop.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(signal.cancelled_owned())
                .await
                .unwrap();
        });
        let (path, base) = config(
            root.path(),
            &origin,
            json!([]),
            json!({"instance_id":"fixture-qq","app_id_env":"FIXTURE_QQ_APP",
            "client_secret_env":"FIXTURE_QQ_SECRET","transport":"webhook","relay":{"api":format!("{origin}/"),"token":format!("{origin}/token")}}),
        );
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reservation.local_addr().unwrap().to_string();
        drop(reservation);
        let mut conf: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        conf["local_instance"] = json!({"name":"Cloud local","workspace":workspace});
        conf["onebot"] = json!({"instance_id":"personal","self_id":"12345","listen":address,"access_token_env":"FIXTURE_ONEBOT_TOKEN"});
        std::fs::write(&path, serde_json::to_vec(&conf).unwrap()).unwrap();
        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let process = Process::launch(&path, base.clone(), &client).await;
        client.post(format!("{base}/api/setup")).header("Origin",&base).json(&json!({"bootstrap_token":SETUP,"login":"owner","password":PASSWORD,"display_name":"Owner"})).send().await.unwrap().error_for_status().unwrap();
        let account = local_instance_tests::browser_login(&process, &client).await;
        Self {
            _root: root,
            path,
            base,
            address,
            workspace,
            process,
            client,
            account,
            wire,
            stop,
            server,
        }
    }
}

#[tokio::test]
async fn personal_and_official_coexist_with_owned_menu_quotes_and_original_approval_loop() {
    let mut f = Fixture::start().await;
    for (token, self_id, role, code) in [
        ("wrong-token", "12345", "Universal", 401),
        (TOKEN, "99999", "Universal", 401),
        (TOKEN, "12345", "Event", 400),
    ] {
        let error = connect_async(request(&f.address, token, self_id, role))
            .await
            .unwrap_err();
        let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
            panic!("wrong error")
        };
        assert_eq!(response.status().as_u16(), code);
    }
    let mut personal = Personal::connect(&f.address).await;
    let mut ambiguous = request(&f.address, TOKEN, "12345", "Universal");
    ambiguous
        .headers_mut()
        .append("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    let invalid = connect_async(ambiguous).await.unwrap_err();
    assert!(
        matches!(invalid,tokio_tungstenite::tungstenite::Error::Http(r) if r.status().as_u16()==401)
    );
    let duplicate = connect_async(request(&f.address, TOKEN, "12345", "Universal"))
        .await
        .unwrap_err();
    assert!(
        matches!(duplicate,tokio_tungstenite::tungstenite::Error::Http(r) if r.status().as_u16()==409)
    );
    // Each adapter performs its own explicit pair into the same account.
    let pair = f
        .process
        .post(&f.client, &f.account, "/api/pairing/code", json!({}))
        .await;
    personal
        .text(1, &format!("/绑定 {}", pair["code"].as_str().unwrap()))
        .await;
    personal.reply("关联").await;
    confirm(&f.process, &f.client, &f.account).await;
    let pair = f
        .process
        .post(&f.client, &f.account, "/api/pairing/code", json!({}))
        .await;
    f.process
        .message(
            &f.client,
            "official-pair",
            &format!("/绑定 {}", pair["code"].as_str().unwrap()),
        )
        .await;
    wait_message(&f.wire, "official-pair", false).await;
    confirm(&f.process, &f.client, &f.account).await;
    let devices = f.process.get(&f.client, &f.account, "/api/account").await;
    let device = devices["devices"][0]["id"].as_str().unwrap();
    f.process
        .message(&f.client, "official-connect", &format!("/连接 {device}"))
        .await;
    wait_message(&f.wire, "official-connect", false).await;
    // Personal route has not inherited the official selection.
    personal.text(2, "/状态").await;
    let (_, status) = personal.reply("选择执行设备").await;
    assert!(status.starts_with("选择执行设备"));
    personal.text(3, &format!("/连接 {device}")).await;
    personal.reply("连接").await;
    personal.text(4, "/菜单").await;
    let (menu, original) = personal.reply("菜单引用：").await;
    assert!(original.contains("/选 1"));
    assert!(!original.contains("点击按钮"));
    // A later menu must not replace the referenced snapshot.
    personal.text(5, "/实例").await;
    personal.reply("菜单引用：").await;
    // Persisted snapshots and action authority survive host and socket restart.
    f.process.stop(&f.client, &f.account).await;
    f.process = Process::launch(&f.path, f.base.clone(), &f.client).await;
    personal.socket = Personal::connect(&f.address).await.socket;
    // Same principal in a different group cannot consume the old action.
    personal.event(6,67890,Some(22222),json!([{"type":"reply","data":{"id":menu.to_string()}},{"type":"text","data":{"text":"1"}}])).await;
    let lookup = personal.call().await;
    assert_eq!(lookup["action"], "get_msg");
    personal
        .response(&lookup, "ok", 0, personal.sent[&menu].clone())
        .await;
    let denied = personal.call().await;
    assert_eq!(denied["params"]["group_id"], 22222);
    assert!(denied["params"]["message"].to_string().contains("过期"));
    personal
        .response(&denied, "ok", 0, json!({"message_id":2000}))
        .await;
    // Original sender/group can quote the original menu exactly once.
    personal.event(7,67890,Some(11111),json!([{"type":"reply","data":{"id":menu.to_string()}},{"type":"text","data":{"text":"1"}}])).await;
    personal.reply("菜单引用：").await;
    // Real model -> Write -> approval broker -> text allow -> executor -> AI.
    personal
        .event(
            8,
            67890,
            Some(11111),
            json!([{"type":"at","data":{"qq":"12345"}},{"type":"text","data":{"text":"写入文件"}}]),
        )
        .await;
    let (_, approval) = personal.reply("确认 1 项操作").await;
    assert!(!approval.contains("PRIVATE_TOOL_CONTENT"));
    assert!(!f.workspace.join("approved.txt").exists());
    let command = approval
        .lines()
        .find_map(|l| l.strip_prefix("允许本次："))
        .unwrap()
        .to_owned();
    personal.text(9, &command).await;
    personal.reply("文件已写入。").await;
    assert_eq!(
        std::fs::read_to_string(f.workspace.join("approved.txt")).unwrap(),
        "PRIVATE_TOOL_CONTENT"
    );
    assert_eq!(f.wire.model_requests.lock().len(), 2);
    // Duplicate platform event cannot invoke the model or send again.
    personal.text(9, &command).await;
    personal.text(10, "/状态").await;
    personal.reply("状态").await;
    assert_eq!(f.wire.model_requests.lock().len(), 2);
    // Shutdown owns and closes a live reverse socket as well as the executor.
    f.process.stop(&f.client, &f.account).await;
    f.stop.cancel();
    f.server.await.unwrap();
}

#[tokio::test]
async fn personal_unknown_send_is_not_replayed_after_reconnect_or_host_restart() {
    let mut f = Fixture::start().await;
    let mut personal = Personal::connect(&f.address).await;
    // Unbound /菜单 produces a notice; drop after receiving the call but before ACK.
    personal.text(20, "/菜单").await;
    let unknown = personal.call().await;
    assert_eq!(unknown["action"], "send_group_msg");
    personal.socket.close(None).await.unwrap();
    // Drain and restart the actual host, preserving the same SQLite state.
    f.process.stop(&f.client, &f.account).await;
    f.process = Process::launch(&f.path, f.base.clone(), &f.client).await;
    let mut again = Personal::connect(&f.address).await;
    again.text(21, "/菜单").await;
    let fresh = again.call().await;
    let reply = fresh["params"]["message"][0]["data"]["id"]
        .as_str()
        .unwrap();
    assert_eq!(reply, "21", "old unknown delivery was replayed");
    again
        .response(&fresh, "ok", 0, json!({"message_id":3000}))
        .await;
    // The original unknown outbox record remains unknown, never delivered.
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(f._root.path().join("state/cloud.sqlite"))
        .read_only(true);
    let pool = sqlx::SqlitePool::connect_with(options).await.unwrap();
    let (count,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM gateway_outbox WHERE state='unconfirmed'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
    pool.close().await;
    f.process.stop(&f.client, &f.account).await;
    f.stop.cancel();
    f.server.await.unwrap();
}
