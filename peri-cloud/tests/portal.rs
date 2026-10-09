use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use peri_cloud::identity::{ChannelIdentity, IdentityService, NativeAuthorization};
use peri_cloud::portal::{self, PortalConfig};
use peri_cloud::state::CloudJournal;
use reqwest::{Client, Response, StatusCode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const SETUP: &str = "fixture-portal-setup-secret-at-least-32-characters";
const PASSWORD: &str = "fixture-owner-password-for-portal";
const VERIFIER: &str = "fixture-native-pkce-verifier-with-at-least-43-characters";

struct Fixture {
    _root: tempfile::TempDir,
    journal: CloudJournal,
    identity: Arc<IdentityService>,
    client: Client,
    base: String,
    origin: String,
    host: String,
    shutdown: CancellationToken,
    server: JoinHandle<std::io::Result<()>>,
}

struct Browser {
    cookies: String,
    csrf: String,
}

impl Fixture {
    async fn new(initialized: bool, https: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let journal = CloudJournal::open(root.path()).await.unwrap();
        let identity = IdentityService::new(&journal, SETUP).await.unwrap();
        if initialized {
            identity
                .initialize(SETUP, "owner", PASSWORD.into(), "Personal owner")
                .await
                .unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let origin = if https {
            "https://agent.example.test".to_owned()
        } else {
            base.clone()
        };
        let host = origin.split_once("://").unwrap().1.to_owned();
        let router = portal::router(identity.clone(), PortalConfig::new(&origin).unwrap());
        let shutdown = CancellationToken::new();
        let cancelled = shutdown.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(cancelled.cancelled_owned())
                .await
        });
        Self {
            _root: root,
            journal,
            identity,
            client: Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            base,
            origin,
            host,
            shutdown,
            server,
        }
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .get(format!("{}{path}", self.base))
            .header("Host", &self.host)
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}{path}", self.base))
            .header("Host", &self.host)
            .header("Origin", &self.origin)
    }

    async fn login(&self) -> (Browser, Response) {
        let response = self
            .post("/api/login")
            .json(&json!({"login": "owner", "password": PASSWORD}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut cookies = Vec::new();
        let mut csrf = String::new();
        for value in response.headers().get_all("set-cookie") {
            let cookie = value.to_str().unwrap().split(';').next().unwrap();
            if cookie.contains("peri_csrf=") {
                csrf = cookie.split_once('=').unwrap().1.to_owned();
            }
            cookies.push(cookie.to_owned());
        }
        assert_eq!(csrf.len(), 64);
        (
            Browser {
                cookies: cookies.join("; "),
                csrf,
            },
            response,
        )
    }

    fn mutation(&self, path: &str, browser: &Browser) -> reqwest::RequestBuilder {
        self.post(path)
            .header("Cookie", &browser.cookies)
            .header("X-Peri-CSRF", &browser.csrf)
    }

    fn authorization(&self, device: Uuid) -> NativeAuthorization {
        NativeAuthorization {
            client_id: "peri-executor".into(),
            device_id: device,
            device_name: "Personal executor".into(),
            redirect_uri: "http://127.0.0.1:45678/oauth/callback".into(),
            code_challenge: URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes())),
            code_challenge_method: "S256".into(),
            state: "fixture-native-random-state".into(),
        }
    }

    async fn grant(&self, browser: &Browser, device: Uuid) -> Value {
        let authorization = self.authorization(device);
        let redirect: Value = self
            .mutation("/oauth/authorize", browser)
            .json(&json!({
                "authorization": authorization, "allow": true,
            }))
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
        let response = self
            .token(&[
                ("grant_type", "authorization_code"),
                ("client_id", "peri-executor"),
                ("code", &code),
                ("code_verifier", VERIFIER),
                ("redirect_uri", &authorization.redirect_uri),
            ])
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }

    async fn token(&self, fields: &[(&str, &str)]) -> Response {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields.iter().copied())
            .finish();
        // Native executors send no browser Cookie or Origin, only PKCE/grants.
        self.client
            .post(format!("{}/oauth/token", self.base))
            .header("Host", &self.host)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .unwrap()
    }

    async fn close(self) {
        self.shutdown.cancel();
        self.server.await.unwrap().unwrap();
        self.journal.close().await;
    }
}

#[tokio::test]
async fn browser_setup_login_cookie_scope_csrf_and_logout_use_real_http() {
    let f = Fixture::new(false, true).await;
    let page = f.get("/").send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert_eq!(page.headers()["cache-control"], "no-store");
    assert_eq!(page.headers()["x-frame-options"], "DENY");
    assert!(page.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .contains("script-src 'self'"));
    assert!(page.text().await.unwrap().contains("id=\"setup-form\""));
    let setup = json!({"bootstrap_token": SETUP, "login": "owner", "password": PASSWORD, "display_name": "Personal owner"});
    let rejected = f
        .post("/api/setup")
        .header("Origin", "https://attacker.example.test")
        .json(&setup)
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
    assert!(!f.identity.initialized().await.unwrap());
    assert_eq!(
        f.post("/api/setup")
            .json(&setup)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let (browser, response) = f.login().await;
    let cookies: Vec<_> = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect();
    assert!(cookies.iter().all(|value| value.contains("SameSite=Strict")
        && value.contains("; Secure")
        && !value.contains("Domain=")));
    assert!(cookies
        .iter()
        .any(|value| value.starts_with("__Host-peri_session=") && value.contains("HttpOnly")));
    assert!(cookies
        .iter()
        .any(|value| value.starts_with("__Host-peri_csrf=") && !value.contains("HttpOnly")));
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["principal"]["login"], "owner");
    assert!(body.get("session_token").is_none());
    assert!(body.get("csrf_token").is_none());
    let denied = f
        .post("/api/pairing/code")
        .header("Cookie", &browser.cookies)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    let account: Value = f
        .get("/api/account")
        .header("Cookie", &browser.cookies)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(account["principal"]["login"], "owner");
    let logout = f
        .mutation("/api/logout", &browser)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(logout.status(), StatusCode::OK);
    assert!(logout
        .headers()
        .get_all("set-cookie")
        .iter()
        .all(|value| value.to_str().unwrap().contains("Max-Age=0")));
    assert_eq!(
        f.get("/api/account")
            .header("Cookie", &browser.cookies)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    f.close().await;
}

#[tokio::test]
async fn native_browser_consent_pkce_token_and_device_registration_are_distinct_from_cookies() {
    let f = Fixture::new(true, false).await;
    let (browser, _) = f.login().await;
    let device = Uuid::new_v4();
    let authorization = f.authorization(device);
    let mut pairs: Vec<(String, String)> = serde_json::to_value(&authorization)
        .unwrap()
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| (key.clone(), value.as_str().unwrap().to_owned()))
        .collect();
    pairs.push(("response_type".into(), "code".into()));
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(
            pairs
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )
        .finish();
    let page = f
        .get(&format!("/oauth/authorize?{query}"))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert!(page.text().await.unwrap().contains("id=\"consent-allow\""));
    assert_eq!(
        f.get(&format!("/oauth/authorize?{query}&response_type=code"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let denial: Value = f
        .mutation("/oauth/authorize", &browser)
        .json(&json!({"authorization": authorization, "allow": false}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(denial["redirect_uri"]
        .as_str()
        .unwrap()
        .contains("error=access_denied"));
    let tokens = f.grant(&browser, device).await;
    assert_eq!(tokens["device_id"], device.to_string());
    let record = json!({"id": device, "name": "Windows executor", "platform": "windows", "default_workspace": "G:\\Project", "connection_id": null, "revoked": false});
    let denied = f
        .mutation("/api/native/device", &browser)
        .json(&record)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    let response = f
        .client
        .post(format!("{}/api/native/device", f.base))
        .header("Host", &f.host)
        .bearer_auth(tokens["access_token"].as_str().unwrap())
        .json(&record)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let receipt: Value = response.json().await.unwrap();
    assert_eq!(receipt["device"]["name"], "Windows executor");
    let mut other = record.clone();
    other["id"] = json!(Uuid::new_v4());
    assert_eq!(
        f.post("/api/native/device")
            .bearer_auth(tokens["access_token"].as_str().unwrap())
            .json(&other)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.mutation(&format!("/api/devices/{device}/revoke"), &browser)
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        f.post("/api/native/device")
            .bearer_auth(tokens["access_token"].as_str().unwrap())
            .json(&record)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let refresh = f
        .token(&[
            ("grant_type", "refresh_token"),
            ("client_id", "peri-executor"),
            ("refresh_token", tokens["refresh_token"].as_str().unwrap()),
        ])
        .await;
    assert_eq!(refresh.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        refresh.json::<Value>().await.unwrap()["error"],
        "invalid_grant"
    );
    f.close().await;
}

#[tokio::test]
async fn gateway_claim_is_not_public_and_exact_browser_confirmation_controls_channel_access() {
    let f = Fixture::new(true, false).await;
    let (browser, _) = f.login().await;
    let code: Value = f
        .mutation("/api/pairing/code", &browser)
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let channel = ChannelIdentity {
        adapter_instance_id: "qq:personal-bot".into(),
        external_user_id: "fixture-qq-owner".into(),
    };
    assert_eq!(
        f.post("/api/pairing/claim")
            .json(&json!({"code": code["code"], "identity": channel}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    // Trusted gateway boundary supplies its observed sender, not browser JSON.
    let claim = f
        .identity
        .claim_pair_code(channel.clone(), code["code"].as_str().unwrap())
        .await
        .unwrap();
    let before: Value = f
        .get("/api/account")
        .header("Cookie", &browser.cookies)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(before["channels"].as_array().unwrap().len(), 0);
    assert_eq!(
        before["pending_pair_claims"][0]["claim_id"],
        claim.claim_id.to_string()
    );
    let path = format!("/api/pairing/{}/confirm", claim.claim_id);
    assert_eq!(
        f.mutation(&path, &browser)
            .json(&json!({"allow": true, "identity": {"external_user_id": "tampered"}}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        f.mutation(&path, &browser)
            .json(&json!({"allow": true}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let after: Value = f
        .get("/api/account")
        .header("Cookie", &browser.cookies)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after["channels"][0]["external_user_id"], "fixture-qq-owner");
    assert_eq!(after["pending_pair_claims"].as_array().unwrap().len(), 0);
    assert_eq!(
        f.mutation(&path, &browser)
            .json(&json!({"allow": true}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.mutation("/api/channels/revoke", &browser)
            .json(&channel)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        f.get("/api/account")
            .header("Cookie", &browser.cookies)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    f.close().await;
}

#[tokio::test]
async fn host_origin_ambiguous_cookie_and_extraction_failures_do_not_expose_credentials() {
    assert!(PortalConfig::new("http://agent.example.test").is_err());
    assert!(PortalConfig::new("https://owner:secret@agent.example.test").is_err());
    let f = Fixture::new(true, false).await;
    let (browser, _) = f.login().await;
    assert_eq!(
        f.get("/api/status")
            .header("Host", "attacker.example.test")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.client
            .post(format!("{}/api/login", f.base))
            .header("Host", &f.host)
            .json(&json!({"login": "owner", "password": PASSWORD}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let duplicated = format!("{}; {}", browser.cookies, browser.cookies);
    assert_eq!(
        f.get("/api/account")
            .header("Cookie", duplicated)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let response = f
        .post("/api/login")
        .header("Content-Type", "application/json")
        .body(format!(
            "{{\"login\":\"owner\",\"password\":\"{PASSWORD}\",\"unexpected\":\"{SETUP}\"}}"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = response.text().await.unwrap();
    assert!(!body.contains(PASSWORD));
    assert!(!body.contains(SETUP));
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["error"],
        "invalid_request"
    );
    let oversized = f
        .post("/api/login")
        .header("Content-Type", "application/json")
        .body("a".repeat(17 * 1024))
        .send()
        .await
        .unwrap();
    assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        oversized.json::<Value>().await.unwrap()["error"],
        "invalid_request"
    );
    f.close().await;
}
