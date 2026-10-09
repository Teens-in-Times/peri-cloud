use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    pub id: Uuid,
    pub login: String,
    pub display_name: String,
}

#[derive(Clone)]
pub struct Authenticated {
    pub(crate) principal: Principal,
    pub(crate) browser: bool,
    pub(crate) device_id: Option<Uuid>,
    pub(crate) credential_hash: String,
}

impl std::fmt::Debug for Authenticated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authenticated")
            .field("principal", &self.principal.id)
            .field("browser", &self.browser)
            .field("device_id", &self.device_id)
            .finish()
    }
}

impl Authenticated {
    pub fn principal(&self) -> &Principal {
        &self.principal
    }
    pub fn device_id(&self) -> Option<Uuid> {
        self.device_id
    }
    pub fn is_browser(&self) -> bool {
        self.browser
    }
}

/// Returned once over the authenticated login response. Do not log/Debug it.
#[derive(Serialize)]
pub struct LoginReceipt {
    pub principal: Principal,
    pub session_token: String,
    pub csrf_token: String,
    pub expires_in: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelIdentity {
    pub adapter_instance_id: String,
    pub external_user_id: String,
}

/// A short-lived code is shown only to the signed-in PC account.
#[derive(Serialize)]
pub struct PairCode {
    pub code: String,
    pub expires_in: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairClaim {
    pub claim_id: Uuid,
    pub identity: ChannelIdentity,
    pub expires_at: i64,
}
