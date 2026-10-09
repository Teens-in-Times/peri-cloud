use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use peri_acp_types::device_enrollment::{DeviceRecord, NativeTokens, RegistrationReceipt};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;

struct FixtureState {
    expected: Mutex<Option<(Uuid, String, String)>>,
    principal: Uuid,
    registrations: AtomicUsize,
}

async fn token(State(state): State<Arc<FixtureState>>, body: Bytes) -> Json<NativeTokens> {
    let fields: HashMap<_, _> = url::form_urlencoded::parse(&body)
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    assert_eq!(fields["grant_type"], "authorization_code");
    assert_eq!(fields["client_id"], "peri-executor");
    assert_eq!(fields["code"], "c".repeat(64));
    let expected = state.expected.lock().await;
    let (device, challenge, redirect) = expected.as_ref().unwrap();
    assert_eq!(&fields["redirect_uri"], redirect);
    assert_eq!(
        URL_SAFE_NO_PAD.encode(Sha256::digest(fields["code_verifier"].as_bytes())),
        *challenge,
        "CLI 发送实际 S256 verifier"
    );
    Json(NativeTokens {
        access_token: "a".repeat(64),
        refresh_token: "b".repeat(64),
        token_type: "Bearer".into(),
        expires_in: 1800,
        principal_id: state.principal,
        device_id: *device,
    })
}

async fn register(
    State(state): State<Arc<FixtureState>>,
    headers: HeaderMap,
    Json(device): Json<DeviceRecord>,
) -> Json<RegistrationReceipt> {
    assert_eq!(
        headers["Authorization"].to_str().unwrap(),
        format!("Bearer {}", "a".repeat(64))
    );
    assert_eq!(device.id, state.expected.lock().await.as_ref().unwrap().0);
    assert!(!device.revoked);
    assert_eq!(device.connection_id, None);
    state.registrations.fetch_add(1, Ordering::SeqCst);
    Json(RegistrationReceipt {
        principal_id: state.principal,
        device,
    })
}

fn command(state_dir: &Path, workspace: &Path, origin: &str, subcommand: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_peri-executor"));
    command.env_clear();
    #[cfg(windows)]
    for name in ["SystemRoot", "WINDIR"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .env("TEMP", workspace)
        .env("TMP", workspace)
        .env("NO_PROXY", "127.0.0.1,localhost");
    command
        .current_dir(workspace)
        .arg("--state-dir")
        .arg(state_dir)
        .arg("--device-name")
        .arg("Fixture CLI device")
        .arg(subcommand)
        .arg("--cloud-url")
        .arg(origin)
        .arg("--workspace")
        .arg(workspace)
        .kill_on_drop(true);
    command
}

struct Cleanup {
    root: std::path::PathBuf,
    workspace: std::path::PathBuf,
    origin: String,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        let root = self.root.clone();
        let workspace = self.workspace.clone();
        let origin = self.origin.clone();
        std::thread::spawn(move || {
            if let Ok(runtime) = tokio::runtime::Runtime::new() {
                runtime.block_on(async {
                    if let Ok(client) = peri_executor::login::LoginClient::new(
                        &origin, &root, "Fixture", &workspace,
                    )
                    .await
                    {
                        let _ = client.forget_local_login().await;
                    }
                });
            }
        })
        .join()
        .unwrap();
    }
}

#[tokio::test]
async fn test_native_cli_login_exit_new_process_reuses_os_credential_and_forget_removes_it() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    let root = directory.path().join("executor");
    std::fs::create_dir(&workspace).unwrap();
    let state = Arc::new(FixtureState {
        expected: Mutex::new(None),
        principal: Uuid::new_v4(),
        registrations: AtomicUsize::new(0),
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let _cleanup = Cleanup {
        root: root.clone(),
        workspace: workspace.clone(),
        origin: origin.clone(),
    };
    let stop = CancellationToken::new();
    let stopped = stop.clone();
    let router = Router::new()
        .route("/oauth/token", post(token))
        .route("/api/native/device", post(register))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(stopped.cancelled_owned())
            .await
    });
    let mut child = command(&root, &workspace, &origin, "login")
        .arg("--no-browser")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    let authorization_url = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let line = lines
                .next_line()
                .await
                .unwrap()
                .expect("CLI 应显示授权入口");
            if line.starts_with("http://") {
                break Url::parse(&line).unwrap();
            }
        }
    })
    .await
    .unwrap();
    let fields: HashMap<_, _> = authorization_url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    assert_eq!(fields["code_challenge_method"], "S256");
    let device = Uuid::parse_str(&fields["device_id"]).unwrap();
    *state.expected.lock().await = Some((
        device,
        fields["code_challenge"].clone(),
        fields["redirect_uri"].clone(),
    ));
    let mut callback = Url::parse(&fields["redirect_uri"]).unwrap();
    callback
        .query_pairs_mut()
        .append_pair("state", &fields["state"])
        .append_pair("code", &"c".repeat(64));
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    assert_eq!(
        http.get(callback).send().await.unwrap().status(),
        reqwest::StatusCode::OK
    );
    let mut output = String::new();
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            output.push_str(&line);
        }
        child.wait().await.unwrap()
    })
    .await
    .unwrap();
    assert!(result.success(), "CLI 登录失败：{result}");
    assert!(output.contains("已授权并登记设备"));
    assert!(
        !output.contains(&"a".repeat(64)) && !output.contains(&"b".repeat(64)),
        "输出不可包含凭据"
    );
    assert_eq!(state.registrations.load(Ordering::SeqCst), 1);
    let status = command(&root, &workspace, &origin, "login-status")
        .output()
        .await
        .unwrap();
    assert!(
        status.status.success(),
        "新进程读取凭据失败：{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["device_id"], device.to_string());
    assert_eq!(status["principal_id"], state.principal.to_string());
    assert!(status.get("access_token").is_none() && status.get("refresh_token").is_none());
    let registration = command(&root, &workspace, &origin, "register")
        .output()
        .await
        .unwrap();
    assert!(registration.status.success());
    assert_eq!(state.registrations.load(Ordering::SeqCst), 2);
    let forgotten = command(&root, &workspace, &origin, "forget-login")
        .output()
        .await
        .unwrap();
    assert!(forgotten.status.success());
    let status = command(&root, &workspace, &origin, "login-status")
        .output()
        .await
        .unwrap();
    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout).contains("未保存登录"));
    stop.cancel();
    server.await.unwrap().unwrap();
}
