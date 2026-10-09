use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Path as RoutePath, State};
use axum::http::header::CONTENT_TYPE;
use axum::routing::{post, put};
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use parking_lot::Mutex;
use peri_executor::{web, Executor};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const SETUP: &str = "fixture-host-bootstrap-at-least-32-characters";
const PASSWORD: &str = "fixture-host-owner-password";
const VERIFIER: &str = "fixture-host-native-verifier-at-least-43-characters";
const QQ_SECRET: &str = "fixture-host-qq-secret-32-characters";
const MODEL_KEY: &str = "fixture-host-model-private-key";

#[derive(Default)]
struct Wire {
    messages: Mutex<Vec<Value>>,
    acknowledgements: Mutex<Vec<(String, Value)>>,
    model_requests: Mutex<Vec<Value>>,
}

async fn outgoing(State(wire): State<Arc<Wire>>, Json(body): Json<Value>) -> Json<Value> {
    wire.messages.lock().push(body);
    Json(json!({"id":"outgoing-fixture-id"}))
}

async fn acknowledge(
    State(wire): State<Arc<Wire>>,
    RoutePath(id): RoutePath<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    wire.acknowledgements.lock().push((id, body));
    Json(json!({}))
}

async fn model(
    State(wire): State<Arc<Wire>>,
    Json(body): Json<Value>,
) -> ([(axum::http::HeaderName, &'static str); 1], String) {
    let index = {
        let mut requests = wire.model_requests.lock();
        let index = requests.len();
        requests.push(body);
        index
    };
    let delta = match index {
        0 | 2 | 4 => {
            let (tool, input) = match index {
                0 => (
                    "Write",
                    json!({"file_path":"approved.txt","content":"PRIVATE_TOOL_CONTENT"}),
                ),
                2 => ("Read", json!({"file_path":"approved.txt"})),
                4 => (
                    "Write",
                    json!({"file_path":"cancelled.txt","content":"MUST_NOT_RUN"}),
                ),
                _ => unreachable!(),
            };
            json!({"choices":[{"delta":{"role":"assistant","tool_calls":[{"index":0,"id":format!("host-tool-{index}"),"type":"function","function":{"name":tool,"arguments":input.to_string()}}]},"finish_reason":"tool_calls"}]})
        }
        1 | 3 => {
            json!({"choices":[{"delta":{"role":"assistant","content":if index == 1 {"文件已写入。"} else {"重启后读取成功。"},"reasoning_content":"PRIVATE_REASONING"},"finish_reason":"stop"}]})
        }
        _ => panic!("unexpected model request"),
    };
    (
        [(CONTENT_TYPE, "text/event-stream")],
        format!("data: {delta}\n\ndata: [DONE]\n\n"),
    )
}

struct Account {
    cookie: String,
    csrf: String,
    access: String,
    principal: String,
}

struct Process {
    child: Child,
    base: String,
}

impl Process {
    async fn launch(path: &Path, base: String, client: &Client) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_peri-cloud-host"));
        command
            .arg("--config")
            .arg(path)
            .env_clear()
            .env("FIXTURE_BOOTSTRAP", SETUP)
            .env("FIXTURE_MODEL_KEY", MODEL_KEY)
            .env("FIXTURE_QQ_APP", "123")
            .env("FIXTURE_QQ_SECRET", QQ_SECRET)
            .env("RUST_LOG", "info")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("TEMP", path.parent().unwrap())
            .env("TMP", path.parent().unwrap())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for name in ["SystemRoot", "WINDIR"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let mut child = command.spawn().unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "cloud host exited before readiness"
                );
                if client
                    .get(format!("{base}/healthz"))
                    .send()
                    .await
                    .is_ok_and(|response| response.status() == StatusCode::OK)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("cloud host did not start");
        Self { child, base }
    }

    async fn account(&self, client: &Client, device: Uuid, workspace: &str) -> Account {
        let setup = client.post(format!("{}/api/setup", self.base)).header("Origin", &self.base)
            .json(&json!({"bootstrap_token":SETUP,"login":"owner","password":PASSWORD,"display_name":"Host owner"}))
            .send().await.unwrap();
        assert_eq!(setup.status(), StatusCode::OK);
        let login = client
            .post(format!("{}/api/login", self.base))
            .header("Origin", &self.base)
            .json(&json!({"login":"owner","password":PASSWORD}))
            .send()
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::OK);
        let cookies: Vec<_> = login
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|value| {
                value
                    .to_str()
                    .unwrap()
                    .split(';')
                    .next()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        let mut account = Account {
            cookie: cookies.join("; "),
            csrf: cookies
                .iter()
                .find(|value| value.starts_with("peri_csrf="))
                .unwrap()
                .split_once('=')
                .unwrap()
                .1
                .to_owned(),
            access: String::new(),
            principal: login.json::<Value>().await.unwrap()["principal"]["id"]
                .as_str()
                .unwrap()
                .into(),
        };
        let redirect_uri = "http://127.0.0.1:45678/oauth/callback";
        let authorization = json!({"client_id":"peri-executor","device_id":device,"device_name":"Host PC","redirect_uri":redirect_uri,
            "code_challenge":URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes())),"code_challenge_method":"S256","state":"fixture-host-native-state"});
        let redirect = self
            .post(
                client,
                &account,
                "/oauth/authorize",
                json!({"authorization":authorization,"allow":true}),
            )
            .await;
        let redirect = url::Url::parse(redirect["redirect_uri"].as_str().unwrap()).unwrap();
        let code = redirect
            .query_pairs()
            .find(|(name, _)| name == "code")
            .unwrap()
            .1
            .into_owned();
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("grant_type", "authorization_code"),
                ("client_id", "peri-executor"),
                ("code", &code),
                ("code_verifier", VERIFIER),
                ("redirect_uri", redirect_uri),
            ])
            .finish();
        let tokens: Value = client
            .post(format!("{}/oauth/token", self.base))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        account.access = tokens["access_token"].as_str().unwrap().into();
        self.register(client, &account, device, workspace).await;
        account
    }

    async fn register(&self, client: &Client, account: &Account, device: Uuid, workspace: &str) {
        let response = client.post(format!("{}/api/native/device", self.base)).bearer_auth(&account.access)
            .json(&json!({"id":device,"name":"Host PC","platform":if cfg!(windows) {"windows"} else {"linux"},
                "default_workspace":workspace,"connection_id":null,"revoked":false}))
            .send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    async fn post(&self, client: &Client, account: &Account, path: &str, body: Value) -> Value {
        client
            .post(format!("{}{path}", self.base))
            .header("Origin", &self.base)
            .header("Cookie", &account.cookie)
            .header("X-Peri-CSRF", &account.csrf)
            .json(&body)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn get(&self, client: &Client, account: &Account, path: &str) -> Value {
        client
            .get(format!("{}{path}", self.base))
            .header("Cookie", &account.cookie)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn message(&self, client: &Client, id: &str, text: &str) {
        self.event(
            client,
            "C2C_MESSAGE_CREATE",
            json!({"id":id,"content":text,"author":{"user_openid":"owner-openid"}}),
        )
        .await;
    }

    async fn event(&self, client: &Client, kind: &str, data: Value) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let body = json!({"op":0,"t":kind,"d":data}).to_string();
        let mut seed = [0; 32];
        for (index, byte) in seed.iter_mut().enumerate() {
            *byte = QQ_SECRET.as_bytes()[index % QQ_SECRET.len()];
        }
        let signature = SigningKey::from_bytes(&seed).sign(format!("{timestamp}{body}").as_bytes());
        let signature: String = signature
            .to_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let response = client
            .post(format!("{}/qq/events", self.base))
            .header("X-Bot-Appid", "123")
            .header("X-Signature-Timestamp", timestamp)
            .header("X-Signature-Ed25519", signature)
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.json::<Value>().await.unwrap(), json!({"op":12}));
    }

    async fn stop(mut self, client: &Client, account: &Account) {
        assert_eq!(
            self.post(client, account, "/api/service/shutdown", json!({}))
                .await["status"],
            "shutdown_requested"
        );
        let status = tokio::time::timeout(Duration::from_secs(15), self.child.wait())
            .await
            .expect("cloud host failed to drain")
            .unwrap();
        let mut log = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut log)
            .await
            .unwrap();
        assert!(status.success(), "cloud host exit: {status}; {log}");
        assert!(log.contains("cloud service stopped after owned work was settled"));
        for secret in [
            SETUP,
            PASSWORD,
            VERIFIER,
            QQ_SECRET,
            MODEL_KEY,
            &account.access,
            &account.csrf,
        ] {
            assert!(!log.contains(secret), "credential appeared in service log");
        }
    }
}

async fn wait_message(wire: &Wire, reply_to: &str, keyboard: bool) -> Value {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(body) = wire
                .messages
                .lock()
                .iter()
                .find(|body| {
                    body["msg_id"] == reply_to && body.get("keyboard").is_some() == keyboard
                })
                .cloned()
            {
                break body;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("QQ delivery not produced")
}

fn config(
    root: &Path,
    model_origin: &str,
    devices: Value,
    qq: Value,
) -> (std::path::PathBuf, String) {
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let base = format!("http://{address}");
    let path = root.join("host.json");
    std::fs::write(&path, serde_json::to_vec(&json!({
        "state_dir":"state","listen":address.to_string(),"public_origin":base,"bootstrap_env":"FIXTURE_BOOTSTRAP",
        "model":{"api_base":format!("{model_origin}/v1"),"api_key_env":"FIXTURE_MODEL_KEY","model":"fixture-model"},
        "devices":devices,"qq":qq,"max_iterations":8
    })).unwrap()).unwrap();
    (path, base)
}

#[tokio::test]
async fn actual_host_qq_buttons_execute_then_restart_restores_session_and_shutdown_cancels_approval(
) {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executor_root = tempfile::tempdir().unwrap();
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let stop = CancellationToken::new();
    let executor = Arc::new(
        Executor::open(executor_root.path(), "Host PC")
            .await
            .unwrap(),
    );
    let device = executor.info().device_id;
    let token_file = executor_root.path().join("transport-token");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = listener.local_addr().unwrap();
    let app = web::router(executor.clone());
    let signal = stop.clone();
    let executor_server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(signal.cancelled_owned())
            .await
            .unwrap();
    });
    let wire = Arc::new(Wire::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route(
            "/token",
            post(|| async { Json(json!({"access_token":"fixture-qq-token","expires_in":3600})) }),
        )
        .route("/v2/users/{user}/messages", post(outgoing))
        .route("/interactions/{id}", put(acknowledge))
        .route("/v1/chat/completions", post(model))
        .with_state(wire.clone());
    let signal = stop.clone();
    let wire_server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(signal.cancelled_owned())
            .await
            .unwrap();
    });
    let (path, base) = config(
        root.path(),
        &origin,
        json!([{"device_id":device,"endpoint":endpoint.to_string(),"token_file":token_file}]),
        json!({"instance_id":"fixture-qq","app_id_env":"FIXTURE_QQ_APP","client_secret_env":"FIXTURE_QQ_SECRET",
            "transport":"webhook","relay":{"api":format!("{origin}/"),"token":format!("{origin}/token")}}),
    );
    let first = Process::launch(&path, base.clone(), &client).await;
    let workspace_path = workspace.path().to_str().unwrap();
    let account = first.account(&client, device, workspace_path).await;
    let pair = first
        .post(&client, &account, "/api/pairing/code", json!({}))
        .await;
    first
        .message(
            &client,
            "pair",
            &format!("/绑定 {}", pair["code"].as_str().unwrap()),
        )
        .await;
    let claims = first.get(&client, &account, "/api/account").await;
    assert_eq!(
        claims["pending_pair_claims"][0]["identity"]["external_user_id"],
        "owner-openid"
    );
    let claim = claims["pending_pair_claims"][0]["claim_id"]
        .as_str()
        .unwrap();
    first
        .post(
            &client,
            &account,
            &format!("/api/pairing/{claim}/confirm"),
            json!({"allow":true}),
        )
        .await;
    first
        .message(&client, "select", &format!("/连接 {device}"))
        .await;
    let selected = wait_message(&wire, "select", false).await;
    assert!(selected["content"]
        .as_str()
        .unwrap()
        .contains("已连接 Host PC"));
    first.message(&client, "write", "创建文件").await;
    let card = wait_message(&wire, "write", true).await;
    assert_eq!(card["msg_type"], 2);
    assert!(card["markdown"].get("content").is_some());
    assert!(card["markdown"].get("custom_template_id").is_none());
    // 回归：客户端不按指定用户列表阻止点击，后台仍拒绝其他点击者及重复审批。
    for button in card["keyboard"]["content"]["rows"][0]["buttons"]
        .as_array()
        .unwrap()
    {
        assert_eq!(button["action"]["permission"], json!({"type":2}));
    }
    assert!(!workspace.path().join("approved.txt").exists());
    let button = card["keyboard"]["content"]["rows"][0]["buttons"][0]["action"]["data"].clone();
    for (id, user, expected) in [
        ("wrong-click", "other-openid", 4),
        ("owner-click", "owner-openid", 0),
        ("duplicate-click", "owner-openid", 4),
    ] {
        first
            .event(
                &client,
                "INTERACTION_CREATE",
                json!({"id":id,"chat_type":2,"user_openid":user,
            "data":{"type":11,"resolved":{"button_data":button}}}),
            )
            .await;
        assert!(wire
            .acknowledgements
            .lock()
            .iter()
            .any(|(event, body)| event == id && body["code"] == expected));
        if user == "other-openid" {
            assert!(!workspace.path().join("approved.txt").exists());
        }
    }
    let reply = wait_message(&wire, "write", false).await;
    assert_eq!(reply["content"], "文件已写入。");
    assert_eq!(reply["msg_type"], 0);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("approved.txt")).unwrap(),
        "PRIVATE_TOOL_CONTENT"
    );
    assert!(!reply.to_string().contains("PRIVATE_TOOL_CONTENT"));
    assert!(!reply.to_string().contains("PRIVATE_REASONING"));
    first.stop(&client, &account).await;

    let second = Process::launch(&path, base, &client).await;
    let restored = second.get(&client, &account, "/api/account").await;
    assert_eq!(restored["principal"]["id"], account.principal);
    assert_eq!(restored["devices"][0]["id"], device.to_string());
    assert_eq!(restored["channels"][0]["external_user_id"], "owner-openid");
    second
        .register(&client, &account, device, workspace_path)
        .await;
    second
        .message(&client, "read-after-restart", "读取刚才的文件")
        .await;
    assert_eq!(
        wait_message(&wire, "read-after-restart", false).await["content"],
        "重启后读取成功。"
    );
    {
        let requests = wire.model_requests.lock();
        assert_eq!(requests.len(), 4);
        let history = requests[2]["messages"].to_string();
        assert!(history.contains("创建文件"));
        assert!(history.contains("文件已写入。"));
        assert!(history.contains("PRIVATE_TOOL_CONTENT"));
        assert!(requests[3]["messages"]
            .to_string()
            .contains("PRIVATE_TOOL_CONTENT"));
        assert!(!requests[2]
            .to_string()
            .contains(&std::fs::read_to_string(&token_file).unwrap()));
    }
    second
        .message(&client, "cancel-on-stop", "再创建一个文件")
        .await;
    wait_message(&wire, "cancel-on-stop", true).await;
    assert!(!workspace.path().join("cancelled.txt").exists());
    second.stop(&client, &account).await;
    assert!(!workspace.path().join("cancelled.txt").exists());
    assert_eq!(wire.model_requests.lock().len(), 5);
    executor.shutdown().await.unwrap();
    stop.cancel();
    executor_server.await.unwrap();
    wire_server.await.unwrap();
}

#[tokio::test]
async fn actual_host_without_qq_and_offline_device_preserves_pc_oauth_and_rejects_unauthorized_stop(
) {
    let root = tempfile::tempdir().unwrap();
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let device = Uuid::new_v4();
    let (path, base) = config(
        root.path(),
        "http://127.0.0.1:9",
        json!([{"device_id":device,"endpoint":"127.0.0.1:9","token_file":"not-yet-enrolled-token"}]),
        Value::Null,
    );
    let host = Process::launch(&path, base, &client).await;
    let status: Value = client
        .get(format!("{}/api/status", host.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["service_control"], true);
    assert_eq!(
        client
            .post(format!("{}/qq/events", host.base))
            .header("Origin", &host.base)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let account = host
        .account(&client, device, root.path().to_str().unwrap())
        .await;
    for (cookie, csrf, origin, expected) in [
        (
            "",
            account.csrf.as_str(),
            host.base.as_str(),
            StatusCode::UNAUTHORIZED,
        ),
        (
            account.cookie.as_str(),
            "wrong",
            host.base.as_str(),
            StatusCode::UNAUTHORIZED,
        ),
        (
            account.cookie.as_str(),
            account.csrf.as_str(),
            "https://other.example.test",
            StatusCode::FORBIDDEN,
        ),
    ] {
        let response = client
            .post(format!("{}/api/service/shutdown", host.base))
            .header("Origin", origin)
            .header("Cookie", cookie)
            .header("X-Peri-CSRF", csrf)
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        assert_eq!(
            client
                .get(format!("{}/healthz", host.base))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }
    assert_eq!(
        host.get(&client, &account, "/api/account").await["devices"][0]["id"],
        device.to_string()
    );
    host.stop(&client, &account).await;
}
