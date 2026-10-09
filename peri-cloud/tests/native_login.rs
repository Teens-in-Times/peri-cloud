use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use peri_cloud::identity::{IdentityClock, IdentityService, NativeAuthorization};
use peri_cloud::portal::{self, PortalConfig};
use peri_cloud::state::CloudJournal;
use peri_executor::login::{LoginClient, LoginError, PendingLogin};
use peri_executor::Executor;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const SETUP: &str = "fixture-native-client-private-setup-at-least-32";
const PASSWORD: &str = "fixture-native-client-password-only";

struct Clock(AtomicI64);
impl IdentityClock for Clock {
    fn unix_seconds(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    journal: CloudJournal,
    identity: Arc<IdentityService>,
    base: String,
    http: reqwest::Client,
    cookies: String,
    csrf: String,
    clock: Arc<Clock>,
    fail_registration: Arc<AtomicBool>,
    lose_refresh_reply: Arc<AtomicBool>,
    token_requests: Arc<AtomicUsize>,
    stop: CancellationToken,
    server: JoinHandle<std::io::Result<()>>,
}

impl Fixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let journal = CloudJournal::open(root.path()).await.unwrap();
        let clock = Arc::new(Clock(AtomicI64::new(1_900_000_000)));
        let identity = IdentityService::with_clock(&journal, SETUP, clock.clone())
            .await
            .unwrap();
        identity
            .initialize(SETUP, "owner", PASSWORD.into(), "Native fixture")
            .await
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let fail_registration = Arc::new(AtomicBool::new(false));
        let fail = fail_registration.clone();
        let lose_refresh_reply = Arc::new(AtomicBool::new(false));
        let lose = lose_refresh_reply.clone();
        let token_requests = Arc::new(AtomicUsize::new(0));
        let requests = token_requests.clone();
        let router = portal::router(identity.clone(), PortalConfig::new(&base).unwrap()).layer(
            middleware::from_fn(move |request: axum::extract::Request, next: Next| {
                let fail = fail.clone();
                let lose = lose.clone();
                let requests = requests.clone();
                async move {
                    if request.uri().path() == "/api/native/device" && fail.load(Ordering::SeqCst) {
                        return reqwest::StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                    let token = request.uri().path() == "/oauth/token";
                    if token {
                        requests.fetch_add(1, Ordering::SeqCst);
                    }
                    let response = next.run(request).await;
                    if token && response.status().is_success() && lose.load(Ordering::SeqCst) {
                        return reqwest::StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                    response
                }
            }),
        );
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(stopped.cancelled_owned())
                .await
        });
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let response = http
            .post(format!("{base}/api/login"))
            .header("Origin", &base)
            .json(&json!({"login": "owner", "password": PASSWORD}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let mut csrf = String::new();
        let cookies = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|value| {
                let cookie = value
                    .to_str()
                    .unwrap()
                    .split(';')
                    .next()
                    .unwrap()
                    .to_owned();
                if cookie.starts_with("peri_csrf=") {
                    csrf = cookie.split_once('=').unwrap().1.into();
                }
                cookie
            })
            .collect::<Vec<_>>()
            .join("; ");
        Self {
            _root: root,
            journal,
            identity,
            base,
            http,
            cookies,
            csrf,
            clock,
            fail_registration,
            lose_refresh_reply,
            token_requests,
            stop,
            server,
        }
    }

    async fn consent(&self, pending: &PendingLogin, allow: bool) {
        let mut fields = serde_json::Map::new();
        for (key, value) in pending.authorization_url().query_pairs() {
            if key != "response_type" {
                fields.insert(key.into_owned(), Value::String(value.into_owned()));
            }
        }
        let authorization: NativeAuthorization =
            serde_json::from_value(Value::Object(fields)).unwrap();
        let response = self
            .http
            .post(format!("{}/oauth/authorize", self.base))
            .header("Origin", &self.base)
            .header("Cookie", &self.cookies)
            .header("X-Peri-CSRF", &self.csrf)
            .json(&json!({"authorization": authorization, "allow": allow}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let response: Value = response.json().await.unwrap();
        let callback = self
            .http
            .get(response["redirect_uri"].as_str().unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(callback.status(), reqwest::StatusCode::OK);
    }

    async fn close(self) {
        self.stop.cancel();
        self.server.await.unwrap().unwrap();
        self.journal.close().await;
    }
}

struct LoginCleanup(LoginClient);
impl Drop for LoginCleanup {
    fn drop(&mut self) {
        let client = self.0.clone();
        // Only this test's unique temporary root/origin/device is removed.
        std::thread::spawn(move || {
            if let Ok(runtime) = tokio::runtime::Runtime::new() {
                let _ = runtime.block_on(client.forget_local_login());
            }
        })
        .join()
        .unwrap();
    }
}

#[tokio::test]
async fn test_real_native_login_reuses_executor_identity_refreshes_and_respects_cloud_revocation() {
    let fixture = Fixture::new().await;
    let device = tempfile::tempdir().unwrap();
    let executor = Executor::open(device.path(), "Native login device")
        .await
        .unwrap();
    let transport = std::fs::read(device.path().join("transport-token")).unwrap();
    let client = LoginClient::new(&fixture.base, device.path(), "ignored-name", device.path())
        .await
        .unwrap();
    let _cleanup = LoginCleanup(client.clone());
    assert_eq!(
        client.device().device_id,
        executor.info().device_id,
        "登录可与常驻执行器共存"
    );
    let pending = client.begin().await.unwrap();
    fixture.consent(&pending, true).await;
    let receipt = pending.finish(Duration::from_secs(2)).await.unwrap();
    assert_eq!(receipt.device.id, executor.info().device_id);
    assert_eq!(receipt.device.name, "Native login device");
    assert_eq!(receipt.device.platform, std::env::consts::OS);
    assert_eq!(
        receipt.device.connection_id, None,
        "登记不伪造 SSH 在线连接"
    );
    assert_eq!(
        transport,
        std::fs::read(device.path().join("transport-token")).unwrap()
    );
    let reopened = LoginClient::new(&fixture.base, device.path(), "ignored-name", device.path())
        .await
        .unwrap();
    let status = reopened.status().await.unwrap();
    let keys = std::fs::read_dir(device.path().join("native-login"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert!(
        status.is_some(),
        "新客户端应读回已保存登录，device={}；凭据命名空间={keys:?}",
        reopened.device().device_id
    );
    assert_eq!(status.unwrap().principal_id, receipt.principal_id);
    fixture.clock.0.fetch_add(1801, Ordering::SeqCst);
    assert_eq!(
        reopened.register_saved().await.unwrap().device.id,
        receipt.device.id,
        "过期 access 由刷新完成登记"
    );
    let response = fixture
        .http
        .post(format!(
            "{}/api/devices/{}/revoke",
            fixture.base, receipt.device.id
        ))
        .header("Origin", &fixture.base)
        .header("Cookie", &fixture.cookies)
        .header("X-Peri-CSRF", &fixture.csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert!(matches!(
        reopened.register_saved().await,
        Err(LoginError::LoginRequired)
    ));
    assert!(
        reopened.status().await.unwrap().is_none(),
        "明确失效的登录应要求重新授权"
    );
    reopened.forget_local_login().await.unwrap();
    assert!(client.status().await.unwrap().is_none());
    executor.shutdown().await.unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn test_lost_refresh_reply_survives_reopen_and_never_replays_rotated_token() {
    let fixture = Fixture::new().await;
    let device = tempfile::tempdir().unwrap();
    let client = LoginClient::new(
        &fixture.base,
        device.path(),
        "Unknown refresh device",
        device.path(),
    )
    .await
    .unwrap();
    let _cleanup = LoginCleanup(client.clone());
    let pending = client.begin().await.unwrap();
    fixture.consent(&pending, true).await;
    pending.finish(Duration::from_secs(2)).await.unwrap();
    fixture.clock.0.fetch_add(1801, Ordering::SeqCst);
    fixture.lose_refresh_reply.store(true, Ordering::SeqCst);
    assert!(matches!(
        client.register_saved().await,
        Err(LoginError::Rejected(503))
    ));
    assert_eq!(fixture.token_requests.load(Ordering::SeqCst), 2);
    let reopened = LoginClient::new(&fixture.base, device.path(), "ignored-name", device.path())
        .await
        .unwrap();
    assert!(
        reopened
            .status()
            .await
            .unwrap()
            .unwrap()
            .reauthorization_required
    );
    assert!(matches!(
        reopened.register_saved().await,
        Err(LoginError::RefreshUncertain)
    ));
    assert_eq!(
        fixture.token_requests.load(Ordering::SeqCst),
        2,
        "不确定结果不能自动重交 refresh token"
    );
    reopened.forget_local_login().await.unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn test_declined_native_consent_does_not_save_a_login_or_device() {
    let fixture = Fixture::new().await;
    let device = tempfile::tempdir().unwrap();
    let client = LoginClient::new(
        &fixture.base,
        device.path(),
        "Declined device",
        device.path(),
    )
    .await
    .unwrap();
    let _cleanup = LoginCleanup(client.clone());
    let pending = client.begin().await.unwrap();
    fixture.consent(&pending, false).await;
    assert!(matches!(
        pending.finish(Duration::from_secs(2)).await,
        Err(LoginError::Declined)
    ));
    assert!(client.status().await.unwrap().is_none());
    let browser = fixture
        .identity
        .login("owner", PASSWORD.into())
        .await
        .unwrap();
    let actor = fixture
        .identity
        .authenticate_browser(&browser.session_token)
        .await
        .unwrap();
    assert!(fixture.identity.devices(&actor).await.unwrap().is_empty());
    fixture.close().await;
}

#[tokio::test]
async fn test_native_registration_failure_preserves_single_use_grant_for_retry() {
    let fixture = Fixture::new().await;
    let device = tempfile::tempdir().unwrap();
    let client = LoginClient::new(&fixture.base, device.path(), "Retry device", device.path())
        .await
        .unwrap();
    let _cleanup = LoginCleanup(client.clone());
    fixture.fail_registration.store(true, Ordering::SeqCst);
    let pending = client.begin().await.unwrap();
    fixture.consent(&pending, true).await;
    assert!(matches!(
        pending.finish(Duration::from_secs(2)).await,
        Err(LoginError::Rejected(503))
    ));
    assert!(
        client.status().await.unwrap().is_some(),
        "换码成功后的登录不可因元数据登记失败而丢失"
    );
    fixture.fail_registration.store(false, Ordering::SeqCst);
    let receipt = client.register_saved().await.unwrap();
    assert_eq!(receipt.device.id, client.device().device_id);
    client.forget_local_login().await.unwrap();
    fixture.close().await;
}
