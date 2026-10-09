use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use peri_acp_types::device_enrollment::{
    DeviceRecord, NativeAuthorization, NativeTokens, RegistrationReceipt, NATIVE_CLIENT_ID,
};
use peri_acp_types::device_executor::ExecutorInfo;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

use super::callback::Callback;
use super::vault::{Credential, Vault};
use super::{LoginError, Result};

#[derive(Clone)]
pub struct LoginClient {
    origin: String,
    root: PathBuf,
    device: ExecutorInfo,
    workspace: String,
    http: reqwest::Client,
}

#[derive(Debug, serde::Serialize)]
pub struct LoginStatus {
    pub cloud_origin: String,
    pub device_id: Uuid,
    pub principal_id: Uuid,
    pub access_expires_at: i64,
    pub reauthorization_required: bool,
}

pub struct PendingLogin {
    client: LoginClient,
    callback: Callback,
    authorization_url: Url,
    verifier: String,
}

impl LoginClient {
    pub async fn new(
        cloud_origin: &str,
        state_dir: &Path,
        name: &str,
        workspace: &Path,
    ) -> Result<Self> {
        let origin = normalize_origin(cloud_origin)?;
        let root = state_dir.to_owned();
        let workspace = workspace.to_owned();
        let name = name.to_owned();
        let (root, device, workspace) = tokio::task::spawn_blocking(move || {
            let device = crate::config::DeviceConfig::enrollment_info(&root, &name)
                .map_err(|_| LoginError::Configuration)?;
            let root = std::fs::canonicalize(root).map_err(|_| LoginError::Configuration)?;
            let workspace =
                std::fs::canonicalize(workspace).map_err(|_| LoginError::Configuration)?;
            if !workspace.is_dir() {
                return Err(LoginError::Configuration);
            }
            let workspace = workspace
                .to_str()
                .ok_or(LoginError::Configuration)?
                .to_owned();
            Ok((root, device, workspace))
        })
        .await
        .map_err(|_| LoginError::Storage)??;
        if device.device_id.is_nil()
            || device.device_name.trim().is_empty()
            || device.device_name.len() > 128
        {
            return Err(LoginError::Configuration);
        }
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10));
        if origin.starts_with("http://") {
            builder = builder.no_proxy();
        }
        let http = builder.build().map_err(|_| LoginError::Configuration)?;
        Ok(Self {
            origin,
            root,
            device,
            workspace,
            http,
        })
    }

    pub fn device(&self) -> &ExecutorInfo {
        &self.device
    }

    pub async fn begin(&self) -> Result<PendingLogin> {
        let verifier = random_secret();
        let state = random_secret();
        let callback = Callback::start(state.clone()).await?;
        let authorization = NativeAuthorization {
            client_id: NATIVE_CLIENT_ID.into(),
            device_id: self.device.device_id,
            device_name: self.device.device_name.clone(),
            redirect_uri: callback.redirect_uri.clone(),
            code_challenge: URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
            code_challenge_method: "S256".into(),
            state,
        };
        authorization
            .validate()
            .map_err(|_| LoginError::Configuration)?;
        let mut authorization_url = Url::parse(&format!("{}/oauth/authorize", self.origin))
            .map_err(|_| LoginError::Configuration)?;
        authorization_url
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &authorization.client_id)
            .append_pair("device_id", &authorization.device_id.to_string())
            .append_pair("device_name", &authorization.device_name)
            .append_pair("redirect_uri", &authorization.redirect_uri)
            .append_pair("code_challenge", &authorization.code_challenge)
            .append_pair(
                "code_challenge_method",
                &authorization.code_challenge_method,
            )
            .append_pair("state", &authorization.state);
        Ok(PendingLogin {
            client: self.clone(),
            callback,
            authorization_url,
            verifier,
        })
    }

    pub async fn status(&self) -> Result<Option<LoginStatus>> {
        let vault = self.vault().await?;
        let origin = self.origin.clone();
        let device_id = self.device.device_id;
        tokio::task::spawn_blocking(move || {
            let Some(credential) = vault.load()? else {
                return Ok(None);
            };
            validate_credential(&credential, &origin, device_id)?;
            Ok(Some(LoginStatus {
                cloud_origin: origin,
                device_id,
                principal_id: credential.principal_id,
                access_expires_at: credential.access_expires_at,
                reauthorization_required: credential.refresh_pending,
            }))
        })
        .await
        .map_err(|_| LoginError::Storage)?
    }

    /// Retry metadata registration from a saved login. This is not proof that
    /// an SSH connection to the device has been established.
    pub async fn register_saved(&self) -> Result<RegistrationReceipt> {
        let vault = self.vault().await?;
        let (vault, credential) = load(vault).await?;
        let mut credential = credential.ok_or(LoginError::LoginRequired)?;
        validate_credential(&credential, &self.origin, self.device.device_id)?;
        if credential.refresh_pending {
            return Err(LoginError::RefreshUncertain);
        }
        let mut vault = vault;
        if credential.access_expires_at <= now().saturating_add(30) {
            (credential, vault) = self.refresh(vault, credential).await?;
        }
        let mut result = self.register(&credential).await;
        if matches!(result, Err(LoginError::Rejected(401))) {
            (credential, vault) = self.refresh(vault, credential).await?;
            result = self.register(&credential).await;
        }
        drop(vault);
        result
    }

    /// Remove this origin/device's local credential only. Use the cloud portal
    /// to revoke the registered device and its remote authorizations.
    pub async fn forget_local_login(&self) -> Result<()> {
        let vault = self.vault().await?;
        tokio::task::spawn_blocking(move || vault.delete())
            .await
            .map_err(|_| LoginError::Storage)?
    }

    async fn register(&self, credential: &Credential) -> Result<RegistrationReceipt> {
        let device = DeviceRecord {
            id: self.device.device_id,
            name: self.device.device_name.clone(),
            platform: self.device.platform.clone(),
            default_workspace: self.workspace.clone(),
            connection_id: None,
            revoked: false,
        };
        let response = self
            .http
            .post(format!("{}/api/native/device", self.origin))
            .bearer_auth(&credential.access_token)
            .json(&device)
            .send()
            .await
            .map_err(|_| LoginError::Network)?;
        let receipt: RegistrationReceipt = decode(response).await?;
        if receipt.principal_id != credential.principal_id
            || receipt.device.id != device.id
            || receipt.device.default_workspace != device.default_workspace
            || receipt.device.revoked
        {
            return Err(LoginError::Protocol);
        }
        Ok(receipt)
    }

    async fn refresh(
        &self,
        vault: Vault,
        mut credential: Credential,
    ) -> Result<(Credential, Vault)> {
        if credential.refresh_pending {
            return Err(LoginError::RefreshUncertain);
        }
        credential.refresh_pending = true;
        let (vault, credential) = save(vault, credential).await?;
        let response = self
            .token(&[
                ("grant_type", "refresh_token"),
                ("client_id", NATIVE_CLIENT_ID),
                ("refresh_token", &credential.refresh_token),
            ])
            .await;
        if matches!(response, Err(LoginError::Rejected(400 | 401))) {
            tokio::task::spawn_blocking(move || vault.delete())
                .await
                .map_err(|_| LoginError::Storage)??;
            return Err(LoginError::LoginRequired);
        }
        let tokens = response.map_err(|error| match error {
            LoginError::Network | LoginError::Protocol => LoginError::RefreshUncertain,
            other => other,
        })?;
        let refreshed = self.credential(tokens)?;
        if refreshed.principal_id != credential.principal_id {
            return Err(LoginError::Protocol);
        }
        let (vault, refreshed) = save(vault, refreshed).await?;
        Ok((refreshed, vault))
    }

    async fn token(&self, fields: &[(&str, &str)]) -> Result<NativeTokens> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields.iter().copied())
            .finish();
        let response = self
            .http
            .post(format!("{}/oauth/token", self.origin))
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await
            .map_err(|_| LoginError::Network)?;
        decode(response).await
    }

    fn credential(&self, tokens: NativeTokens) -> Result<Credential> {
        if tokens.device_id != self.device.device_id
            || tokens.principal_id.is_nil()
            || tokens.token_type != "Bearer"
            || !valid_secret(&tokens.access_token)
            || !valid_secret(&tokens.refresh_token)
            || tokens.expires_in == 0
            || tokens.expires_in > 86400
        {
            return Err(LoginError::Protocol);
        }
        Ok(Credential {
            origin: self.origin.clone(),
            principal_id: tokens.principal_id,
            device_id: tokens.device_id,
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            access_expires_at: now().saturating_add(tokens.expires_in as i64),
            refresh_pending: false,
        })
    }

    async fn vault(&self) -> Result<Vault> {
        let root = self.root.clone();
        let origin = self.origin.clone();
        let device = self.device.device_id;
        tokio::task::spawn_blocking(move || Vault::open(&root, &origin, device))
            .await
            .map_err(|_| LoginError::Storage)?
    }
}

impl PendingLogin {
    pub fn authorization_url(&self) -> &Url {
        &self.authorization_url
    }

    pub async fn finish(mut self, timeout: Duration) -> Result<RegistrationReceipt> {
        let code = self.callback.code(timeout).await?;
        let vault = self.client.vault().await?;
        let tokens = self
            .client
            .token(&[
                ("grant_type", "authorization_code"),
                ("client_id", NATIVE_CLIENT_ID),
                ("code", &code),
                ("code_verifier", &self.verifier),
                ("redirect_uri", &self.callback.redirect_uri),
            ])
            .await?;
        let credential = self.client.credential(tokens)?;
        // Preserve the single-use grant before registration, so a network
        // failure during registration can be retried with register_saved.
        let (_vault, credential) = save(vault, credential).await?;
        self.client.register(&credential).await
    }
}

fn normalize_origin(origin: &str) -> Result<String> {
    let url = Url::parse(origin).map_err(|_| LoginError::Configuration)?;
    if url.host_str().is_none()
        || !(url.scheme() == "https"
            || (url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1" | "[::1]"))))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(LoginError::Configuration);
    }
    Ok(url.origin().ascii_serialization())
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0)
}
fn random_secret() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}
fn valid_secret(secret: &str) -> bool {
    secret.len() == 64 && secret.bytes().all(|byte| byte.is_ascii_hexdigit())
}
fn validate_credential(credential: &Credential, origin: &str, device: Uuid) -> Result<()> {
    if credential.origin != origin
        || credential.device_id != device
        || credential.principal_id.is_nil()
        || !valid_secret(&credential.access_token)
        || !valid_secret(&credential.refresh_token)
    {
        return Err(LoginError::InvalidCredential);
    }
    Ok(())
}
async fn load(vault: Vault) -> Result<(Vault, Option<Credential>)> {
    tokio::task::spawn_blocking(move || {
        let credential = vault.load()?;
        Ok((vault, credential))
    })
    .await
    .map_err(|_| LoginError::Storage)?
}
async fn save(vault: Vault, credential: Credential) -> Result<(Vault, Credential)> {
    tokio::task::spawn_blocking(move || {
        vault.save(&credential)?;
        Ok((vault, credential))
    })
    .await
    .map_err(|_| LoginError::Storage)?
}
async fn decode<T: DeserializeOwned>(mut response: reqwest::Response) -> Result<T> {
    if !response.status().is_success() {
        return Err(LoginError::Rejected(response.status().as_u16()));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| LoginError::Network)? {
        if bytes.len().saturating_add(chunk.len()) > 16 * 1024 {
            return Err(LoginError::Protocol);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| LoginError::Protocol)
}
