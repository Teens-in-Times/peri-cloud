use std::process::Stdio;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use reqwest::Client;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::process::{Child, Command};
use uuid::Uuid;

const SETUP: &str = "fixture-process-setup-secret-at-least-32-characters";
const PASSWORD: &str = "fixture-process-owner-password";
const VERIFIER: &str = "fixture-native-pkce-verifier-with-at-least-43-characters";

async fn launch(root: &std::path::Path, client: &Client) -> (Child, String) {
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let mut command = Command::new(env!("CARGO_BIN_EXE_peri-account-portal"));
    command
        .args(["--listen", &address.to_string(), "--state-dir"])
        .arg(root)
        .env_clear()
        .env("PERI_CLOUD_SETUP_TOKEN", SETUP)
        .env("RUST_LOG", "warn")
        .env("TEMP", root)
        .env("TMP", root)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for name in ["SystemRoot", "WINDIR"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let mut child = command.spawn().unwrap();
    let base = format!("http://{address}");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "account process terminated before readiness"
            );
            if client
                .get(format!("{base}/api/status"))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("account process did not become ready");
    (child, base)
}

#[tokio::test]
async fn actual_account_process_restart_preserves_login_native_grant_and_device() {
    let root = tempfile::tempdir().unwrap();
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let (mut first, base) = launch(root.path(), &client).await;
    let setup = client.post(format!("{base}/api/setup")).header("Origin", &base)
        .json(&json!({"bootstrap_token": SETUP, "login": "owner", "password": PASSWORD, "display_name": "Process owner"}))
        .send().await.unwrap();
    assert_eq!(setup.status(), reqwest::StatusCode::OK);
    let login = client
        .post(format!("{base}/api/login"))
        .header("Origin", &base)
        .json(&json!({"login": "owner", "password": PASSWORD}))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), reqwest::StatusCode::OK);
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
    let cookie = cookies.join("; ");
    let csrf = cookies
        .iter()
        .find(|value| value.starts_with("peri_csrf="))
        .unwrap()
        .split_once('=')
        .unwrap()
        .1
        .to_owned();
    let principal: Value = login.json().await.unwrap();
    let device = Uuid::new_v4();
    let redirect_uri = "http://127.0.0.1:45678/oauth/callback";
    let authorization = json!({"client_id": "peri-executor", "device_id": device, "device_name": "Process executor", "redirect_uri": redirect_uri,
        "code_challenge": URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes())), "code_challenge_method": "S256", "state": "fixture-process-native-state"});
    let redirect: Value = client
        .post(format!("{base}/oauth/authorize"))
        .header("Origin", &base)
        .header("Cookie", &cookie)
        .header("X-Peri-CSRF", &csrf)
        .json(&json!({"authorization": authorization, "allow": true}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let url = url::Url::parse(redirect["redirect_uri"].as_str().unwrap()).unwrap();
    let code = url
        .query_pairs()
        .find(|(key, _)| key == "code")
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
        .post(format!("{base}/oauth/token"))
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
    let record = json!({"id": device, "name": "Persisted executor", "platform": "linux", "default_workspace": "/srv/project", "connection_id": null, "revoked": false});
    assert_eq!(
        client
            .post(format!("{base}/api/native/device"))
            .bearer_auth(tokens["access_token"].as_str().unwrap())
            .json(&record)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    // Abrupt process exit is stronger than dropping/reopening one pool. All
    // acknowledged identity mutations must already be durable and reusable.
    first.start_kill().unwrap();
    first.wait().await.unwrap();
    let (mut second, restarted) = launch(root.path(), &client).await;
    let account: Value = client
        .get(format!("{restarted}/api/account"))
        .header("Cookie", &cookie)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(account["principal"]["id"], principal["principal"]["id"]);
    assert_eq!(account["devices"][0]["id"], device.to_string());
    assert_eq!(account["devices"][0]["name"], "Persisted executor");
    assert_eq!(
        client
            .post(format!("{restarted}/api/native/device"))
            .bearer_auth(tokens["access_token"].as_str().unwrap())
            .json(&record)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(
        client
            .post(format!("{restarted}/api/pairing/code"))
            .header("Origin", &restarted)
            .header("Cookie", &cookie)
            .header("X-Peri-CSRF", &csrf)
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    second.start_kill().unwrap();
    second.wait().await.unwrap();
}
