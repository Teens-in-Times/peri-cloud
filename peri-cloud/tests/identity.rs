use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use peri_cloud::identity::{
    Authenticated, ChannelIdentity, DeviceRecord, IdentityClock, IdentityError, IdentityService,
    NativeAuthorization,
};
use peri_cloud::state::CloudJournal;
use sha2::{Digest, Sha256};
use uuid::Uuid;

const SETUP: &str = "fixture-setup-secret-at-least-32-characters";
const PASSWORD: &str = "fixture-owner-password-for-real-argon2";
const VERIFIER: &str = "fixture-native-pkce-verifier-with-at-least-43-characters";

struct Fixture {
    root: tempfile::TempDir,
    journal: CloudJournal,
    identity: Arc<IdentityService>,
    browser: Authenticated,
    browser_token: String,
    csrf: String,
    clock: Arc<Clock>,
}

struct Clock(AtomicI64);
impl IdentityClock for Clock {
    fn unix_seconds(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}
impl Clock {
    fn advance(&self, seconds: i64) {
        self.0.fetch_add(seconds, Ordering::SeqCst);
    }
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
            .initialize(SETUP, "owner", PASSWORD.into(), "Personal owner")
            .await
            .unwrap();
        let login = identity.login("owner", PASSWORD.into()).await.unwrap();
        let browser = identity
            .authenticate_browser(&login.session_token)
            .await
            .unwrap();
        Self {
            root,
            journal,
            identity,
            browser,
            browser_token: login.session_token,
            csrf: login.csrf_token,
            clock,
        }
    }

    fn authorization(device: Uuid) -> NativeAuthorization {
        NativeAuthorization {
            client_id: "peri-executor".into(),
            device_id: device,
            device_name: "Windows PC".into(),
            redirect_uri: "http://127.0.0.1:45678/oauth/callback".into(),
            code_challenge: URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes())),
            code_challenge_method: "S256".into(),
            state: "fixture-executor-random-state".into(),
        }
    }

    async fn grant(&self, device: Uuid) -> peri_cloud::identity::NativeTokens {
        let auth = Self::authorization(device);
        let redirect = self
            .identity
            .authorize_native(&self.browser, auth.clone())
            .await
            .unwrap();
        let code = code(&redirect);
        self.identity
            .exchange_code("peri-executor", &code, VERIFIER, &auth.redirect_uri)
            .await
            .unwrap()
    }
}

fn code(redirect: &str) -> String {
    url::Url::parse(redirect)
        .unwrap()
        .query_pairs()
        .find(|(key, _)| key == "code")
        .unwrap()
        .1
        .into_owned()
}

fn external(adapter: &str) -> ChannelIdentity {
    ChannelIdentity {
        adapter_instance_id: adapter.into(),
        external_user_id: "fixture-qq-sender".into(),
    }
}

#[tokio::test]
async fn private_setup_password_login_csrf_logout_and_hash_only_storage() {
    let f = Fixture::new().await;
    assert!(matches!(
        f.identity
            .initialize(SETUP, "other", PASSWORD.into(), "Other")
            .await,
        Err(IdentityError::AlreadyInitialized)
    ));
    assert!(matches!(
        f.identity.login("owner", "wrong-password".into()).await,
        Err(IdentityError::Unauthorized)
    ));
    assert!(matches!(
        f.identity.login("missing", PASSWORD.into()).await,
        Err(IdentityError::Unauthorized)
    ));
    f.identity.verify_csrf(&f.browser, &f.csrf).await.unwrap();
    assert!(matches!(
        f.identity.verify_csrf(&f.browser, "wrong-csrf").await,
        Err(IdentityError::Unauthorized)
    ));
    let dump = std::fs::read(f.root.path().join("cloud.sqlite-wal")).unwrap();
    for plaintext in [PASSWORD, f.browser_token.as_str(), f.csrf.as_str()] {
        assert!(!dump
            .windows(plaintext.len())
            .any(|bytes| bytes == plaintext.as_bytes()));
    }
    let actor_debug = format!("{:?}", f.browser);
    assert!(!actor_debug.contains(&f.browser_token));
    f.identity.logout(&f.browser).await.unwrap();
    assert!(matches!(
        f.identity.authenticate_browser(&f.browser_token).await,
        Err(IdentityError::Unauthorized)
    ));
    assert!(matches!(
        f.identity.create_pair_code(&f.browser).await,
        Err(IdentityError::Unauthorized)
    ));
    f.journal.close().await;
}

#[tokio::test]
async fn native_pkce_exact_redirect_single_use_and_device_bound_scope() {
    let f = Fixture::new().await;
    let device = Uuid::new_v4();
    let auth = Fixture::authorization(device);
    let redirect = f
        .identity
        .authorize_native(&f.browser, auth.clone())
        .await
        .unwrap();
    let redirect_url = url::Url::parse(&redirect).unwrap();
    assert_eq!(
        redirect_url
            .query_pairs()
            .find(|(key, _)| key == "state")
            .unwrap()
            .1,
        auth.state
    );
    let code = code(&redirect);
    assert!(matches!(
        f.identity
            .exchange_code(
                "peri-executor",
                &code,
                "different-pkce-verifier-with-at-least-43-characters",
                &auth.redirect_uri
            )
            .await,
        Err(IdentityError::Unauthorized)
    ));
    assert!(matches!(
        f.identity
            .exchange_code(
                "peri-executor",
                &code,
                VERIFIER,
                "http://127.0.0.1:45679/oauth/callback"
            )
            .await,
        Err(IdentityError::Unauthorized)
    ));
    let tokens = f
        .identity
        .exchange_code("peri-executor", &code, VERIFIER, &auth.redirect_uri)
        .await
        .unwrap();
    assert!(matches!(
        f.identity
            .exchange_code("peri-executor", &code, VERIFIER, &auth.redirect_uri)
            .await,
        Err(IdentityError::Unauthorized)
    ));
    let actor = f
        .identity
        .authenticate_native(&tokens.access_token)
        .await
        .unwrap();
    let record = DeviceRecord {
        id: device,
        name: "Personal PC".into(),
        platform: "windows".into(),
        default_workspace: "G:\\Project".into(),
        connection_id: None,
        revoked: false,
    };
    f.identity
        .register_device(&actor, record.clone())
        .await
        .unwrap();
    let mut other_device = record;
    other_device.id = Uuid::new_v4();
    assert!(matches!(
        f.identity.register_device(&actor, other_device).await,
        Err(IdentityError::Forbidden)
    ));
    assert!(matches!(
        f.identity.create_pair_code(&actor).await,
        Err(IdentityError::Forbidden)
    ));
    assert_eq!(f.identity.devices(&f.browser).await.unwrap()[0].id, device);
    f.journal.close().await;
}

#[tokio::test]
async fn native_redirect_and_pkce_downgrade_are_rejected_before_code_issue() {
    let f = Fixture::new().await;
    for redirect in [
        "https://example.test/oauth/callback",
        "http://localhost:45678/oauth/callback",
        "http://127.0.0.1:45678/other",
        "http://127.0.0.1:45678/oauth/callback?unexpected=1",
    ] {
        let mut auth = Fixture::authorization(Uuid::new_v4());
        auth.redirect_uri = redirect.into();
        assert!(matches!(
            f.identity.authorize_native(&f.browser, auth).await,
            Err(IdentityError::Invalid)
        ));
    }
    let mut auth = Fixture::authorization(Uuid::new_v4());
    auth.code_challenge_method = "plain".into();
    assert!(matches!(
        f.identity.authorize_native(&f.browser, auth).await,
        Err(IdentityError::Invalid)
    ));
    f.journal.close().await;
}

#[tokio::test]
async fn refresh_rotation_replay_revokes_family_and_device_revocation_invalidates_pending_codes() {
    let f = Fixture::new().await;
    let device = Uuid::new_v4();
    let first = f.grant(device).await;
    let rotated = f
        .identity
        .refresh_native("peri-executor", &first.refresh_token)
        .await
        .unwrap();
    assert!(matches!(
        f.identity.authenticate_native(&first.access_token).await,
        Err(IdentityError::Unauthorized)
    ));
    f.identity
        .authenticate_native(&rotated.access_token)
        .await
        .unwrap();
    assert!(matches!(
        f.identity
            .refresh_native("peri-executor", &first.refresh_token)
            .await,
        Err(IdentityError::Unauthorized)
    ));
    assert!(matches!(
        f.identity.authenticate_native(&rotated.access_token).await,
        Err(IdentityError::Unauthorized)
    ));
    let new = f.grant(device).await;
    let actor = f
        .identity
        .authenticate_native(&new.access_token)
        .await
        .unwrap();
    f.identity
        .register_device(
            &actor,
            DeviceRecord {
                id: device,
                name: "Linux executor".into(),
                platform: "linux".into(),
                default_workspace: "/srv/project".into(),
                connection_id: None,
                revoked: false,
            },
        )
        .await
        .unwrap();
    let auth = Fixture::authorization(device);
    let pending = f
        .identity
        .authorize_native(&f.browser, auth.clone())
        .await
        .unwrap();
    f.identity.revoke_device(&f.browser, device).await.unwrap();
    assert!(matches!(
        f.identity.authenticate_native(&new.access_token).await,
        Err(IdentityError::Unauthorized)
    ));
    assert!(matches!(
        f.identity
            .exchange_code(
                "peri-executor",
                &code(&pending),
                VERIFIER,
                &auth.redirect_uri
            )
            .await,
        Err(IdentityError::Unauthorized)
    ));
    f.journal.close().await;
}

#[tokio::test]
async fn qq_pairing_requires_pc_confirmation_is_single_use_and_adapter_scoped() {
    let f = Fixture::new().await;
    let pair = f.identity.create_pair_code(&f.browser).await.unwrap();
    assert_eq!(pair.expires_in, 300);
    let qq = external("qq:personal-bot");
    let claim = f
        .identity
        .claim_pair_code(qq.clone(), &pair.code)
        .await
        .unwrap();
    assert!(matches!(
        f.identity.resolve_channel(&qq).await,
        Err(IdentityError::Unauthorized)
    ));
    assert!(matches!(
        f.identity
            .claim_pair_code(external("qq:another-bot"), &pair.code)
            .await,
        Err(IdentityError::Unauthorized)
    ));
    assert_eq!(
        f.identity.pending_pair_claims(&f.browser).await.unwrap()[0].claim_id,
        claim.claim_id
    );
    f.identity
        .confirm_pair_claim(&f.browser, claim.claim_id, true)
        .await
        .unwrap();
    assert_eq!(
        f.identity.resolve_channel(&qq).await.unwrap(),
        f.browser.principal().id
    );
    assert!(matches!(
        f.identity
            .resolve_channel(&external("qq:another-bot"))
            .await,
        Err(IdentityError::Unauthorized)
    ));
    assert!(matches!(
        f.identity
            .confirm_pair_claim(&f.browser, claim.claim_id, true)
            .await,
        Err(IdentityError::Stale)
    ));
    f.identity.revoke_channel(&f.browser, &qq).await.unwrap();
    assert!(matches!(
        f.identity.resolve_channel(&qq).await,
        Err(IdentityError::Unauthorized)
    ));
    // Disabling/removing QQ does not remove the independent PC login.
    f.identity
        .authenticate_browser(&f.browser_token)
        .await
        .unwrap();
    f.journal.close().await;
}

#[tokio::test]
async fn replacing_pair_code_invalidates_claim_and_rejection_does_not_bind_channel() {
    let f = Fixture::new().await;
    let first = f.identity.create_pair_code(&f.browser).await.unwrap();
    let old = f
        .identity
        .claim_pair_code(external("qq:personal-bot"), &first.code)
        .await
        .unwrap();
    let second = f.identity.create_pair_code(&f.browser).await.unwrap();
    assert!(matches!(
        f.identity
            .confirm_pair_claim(&f.browser, old.claim_id, true)
            .await,
        Err(IdentityError::Stale)
    ));
    let current = f
        .identity
        .claim_pair_code(external("qq:personal-bot"), &second.code)
        .await
        .unwrap();
    f.identity
        .confirm_pair_claim(&f.browser, current.claim_id, false)
        .await
        .unwrap();
    assert!(matches!(
        f.identity.resolve_channel(&current.identity).await,
        Err(IdentityError::Unauthorized)
    ));
    f.journal.close().await;
}

#[tokio::test]
async fn account_session_native_credentials_and_pairing_survive_database_reopen() {
    let f = Fixture::new().await;
    let principal = f.browser.principal().id;
    let device = Uuid::new_v4();
    let tokens = f.grant(device).await;
    let pair = f.identity.create_pair_code(&f.browser).await.unwrap();
    let claim = f
        .identity
        .claim_pair_code(external("qq:personal-bot"), &pair.code)
        .await
        .unwrap();
    f.identity
        .confirm_pair_claim(&f.browser, claim.claim_id, true)
        .await
        .unwrap();
    f.journal.close().await;
    let Fixture {
        root,
        journal,
        identity,
        browser_token,
        clock,
        ..
    } = f;
    drop(identity);
    drop(journal);
    let journal = CloudJournal::open(root.path()).await.unwrap();
    let service = IdentityService::with_clock(&journal, SETUP, clock)
        .await
        .unwrap();
    assert_eq!(
        service
            .authenticate_browser(&browser_token)
            .await
            .unwrap()
            .principal()
            .id,
        principal
    );
    assert_eq!(
        service
            .authenticate_native(&tokens.access_token)
            .await
            .unwrap()
            .device_id(),
        Some(device)
    );
    assert_eq!(
        service
            .resolve_channel(&external("qq:personal-bot"))
            .await
            .unwrap(),
        principal
    );
    journal.close().await;
}

#[tokio::test]
async fn expired_authorization_pair_claim_and_browser_session_cannot_authorize_mutations() {
    let f = Fixture::new().await;
    let authorization = Fixture::authorization(Uuid::new_v4());
    let redirect = f
        .identity
        .authorize_native(&f.browser, authorization.clone())
        .await
        .unwrap();
    let pair = f.identity.create_pair_code(&f.browser).await.unwrap();
    f.clock.advance(120);
    assert!(matches!(
        f.identity
            .exchange_code(
                "peri-executor",
                &code(&redirect),
                VERIFIER,
                &authorization.redirect_uri
            )
            .await,
        Err(IdentityError::Unauthorized)
    ));
    let claim = f
        .identity
        .claim_pair_code(external("qq:personal-bot"), &pair.code)
        .await
        .unwrap();
    f.clock.advance(180);
    assert!(matches!(
        f.identity
            .confirm_pair_claim(&f.browser, claim.claim_id, true)
            .await,
        Err(IdentityError::Stale)
    ));
    assert!(matches!(
        f.identity.resolve_channel(&claim.identity).await,
        Err(IdentityError::Unauthorized)
    ));
    f.clock.advance(12 * 3600 - 300);
    assert!(matches!(
        f.identity.authenticate_browser(&f.browser_token).await,
        Err(IdentityError::Unauthorized)
    ));
    assert!(matches!(
        f.identity.create_pair_code(&f.browser).await,
        Err(IdentityError::Unauthorized)
    ));
    f.journal.close().await;
}

#[tokio::test]
async fn native_access_expiry_refresh_and_absolute_family_expiry_are_independent() {
    let f = Fixture::new().await;
    let tokens = f.grant(Uuid::new_v4()).await;
    f.clock.advance(30 * 60);
    assert!(matches!(
        f.identity.authenticate_native(&tokens.access_token).await,
        Err(IdentityError::Unauthorized)
    ));
    let renewed = f
        .identity
        .refresh_native("peri-executor", &tokens.refresh_token)
        .await
        .unwrap();
    f.identity
        .authenticate_native(&renewed.access_token)
        .await
        .unwrap();
    f.clock.advance(30 * 24 * 3600 - 30 * 60);
    assert!(matches!(
        f.identity
            .refresh_native("peri-executor", &renewed.refresh_token)
            .await,
        Err(IdentityError::Unauthorized)
    ));
    f.journal.close().await;
}

#[tokio::test]
async fn login_rate_limit_resets_only_after_its_window_and_account_scope_is_preserved() {
    let f = Fixture::new().await;
    // The fixture's successful login already consumed one attempt.
    for _ in 0..7 {
        assert!(matches!(
            f.identity.login("owner", "wrong-password".into()).await,
            Err(IdentityError::Unauthorized)
        ));
    }
    assert!(matches!(
        f.identity.login("owner", PASSWORD.into()).await,
        Err(IdentityError::RateLimited)
    ));
    f.clock.advance(59);
    assert!(matches!(
        f.identity.login("owner", PASSWORD.into()).await,
        Err(IdentityError::RateLimited)
    ));
    f.clock.advance(1);
    assert_eq!(
        f.identity
            .login("owner", PASSWORD.into())
            .await
            .unwrap()
            .principal
            .id,
        f.browser.principal().id
    );
    f.journal.close().await;
}

#[tokio::test]
async fn pairing_attempt_limit_is_scoped_to_sender_and_expiry_does_not_bind_identity() {
    let f = Fixture::new().await;
    for _ in 0..8 {
        assert!(matches!(
            f.identity
                .claim_pair_code(external("qq:personal-bot"), "000000000000")
                .await,
            Err(IdentityError::Unauthorized)
        ));
    }
    let pair = f.identity.create_pair_code(&f.browser).await.unwrap();
    assert!(matches!(
        f.identity
            .claim_pair_code(external("qq:personal-bot"), &pair.code)
            .await,
        Err(IdentityError::RateLimited)
    ));
    let claim = f
        .identity
        .claim_pair_code(external("qq:another-bot"), &pair.code)
        .await
        .unwrap();
    f.clock.advance(300);
    assert!(matches!(
        f.identity
            .confirm_pair_claim(&f.browser, claim.claim_id, true)
            .await,
        Err(IdentityError::Stale)
    ));
    assert!(matches!(
        f.identity.resolve_channel(&claim.identity).await,
        Err(IdentityError::Unauthorized)
    ));
    f.journal.close().await;
}
