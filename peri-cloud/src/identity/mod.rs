//! Cloud accounts, native OAuth grants and independently revocable channels.
//! Credentials never enter Agent/tool parameters or Debug representations.

mod channel;
mod clock;
mod crypto;
mod oauth;
mod pairing;
mod service;
pub(crate) mod store;
mod types;

pub use clock::IdentityClock;
pub use peri_acp_types::device_enrollment::{
    DeviceRecord, NativeAuthorization, NativeTokens, RegistrationReceipt,
};
pub use service::IdentityService;
pub use types::{Authenticated, ChannelIdentity, LoginReceipt, PairClaim, PairCode, Principal};

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("identity state is unavailable")]
    Database(#[from] sqlx::Error),
    #[error("identity record is invalid")]
    Encoding(#[from] serde_json::Error),
    #[error("identity credential is invalid or expired")]
    Unauthorized,
    #[error("this identity does not have the required account scope")]
    Forbidden,
    #[error("identity input is invalid")]
    Invalid,
    #[error("cloud account has already been initialized")]
    AlreadyInitialized,
    #[error("identity is already associated with another account")]
    OwnershipConflict,
    #[error("identity request is stale or already settled")]
    Stale,
    #[error("identity request rate limit was reached")]
    RateLimited,
    #[error("identity operation failed")]
    Crypto,
}

pub type IdentityResult<T> = Result<T, IdentityError>;
