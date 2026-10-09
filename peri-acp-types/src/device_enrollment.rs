//! Native device enrollment contracts shared by cloud identity and executors.
//! Tokens intentionally have no Debug implementation.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const NATIVE_CLIENT_ID: &str = "peri-executor";

#[derive(Debug, thiserror::Error)]
#[error("invalid native device authorization")]
pub struct InvalidAuthorization;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeAuthorization {
    pub client_id: String,
    pub device_id: Uuid,
    pub device_name: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub state: String,
}

impl NativeAuthorization {
    pub fn validate(&self) -> Result<(), InvalidAuthorization> {
        if self.client_id != NATIVE_CLIENT_ID
            || self.code_challenge_method != "S256"
            || !(16..=256).contains(&self.state.len())
            || self.device_name.trim().is_empty()
            || self.device_name.len() > 128
            || URL_SAFE_NO_PAD
                .decode(&self.code_challenge)
                .map_or(true, |bytes| bytes.len() != 32)
        {
            return Err(InvalidAuthorization);
        }
        let redirect = url::Url::parse(&self.redirect_uri).map_err(|_| InvalidAuthorization)?;
        if redirect.scheme() != "http"
            || !matches!(redirect.host_str(), Some("127.0.0.1" | "[::1]"))
            || redirect.port().unwrap_or(0) == 0
            || redirect.path() != "/oauth/callback"
            || !redirect.username().is_empty()
            || redirect.password().is_some()
            || redirect.query().is_some()
            || redirect.fragment().is_some()
        {
            return Err(InvalidAuthorization);
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
pub struct NativeTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_in: u64,
    pub principal_id: Uuid,
    pub device_id: Uuid,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceRecord {
    pub id: Uuid,
    pub name: String,
    pub platform: String,
    pub default_workspace: String,
    /// Set only by deployment-owned connection configuration.
    pub connection_id: Option<Uuid>,
    pub revoked: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegistrationReceipt {
    pub principal_id: Uuid,
    pub device: DeviceRecord,
}

#[cfg(test)]
#[path = "device_enrollment_test.rs"]
mod tests;
