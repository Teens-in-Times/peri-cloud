//! Browser PKCE enrollment, separate from executor transport credentials.
//! Native tokens are never returned to CLI output or model tool arguments.

mod callback;
mod client;
#[cfg(unix)]
mod unix;
mod vault;
#[cfg(windows)]
mod windows;

pub use client::{LoginClient, LoginStatus, PendingLogin};
pub use peri_acp_types::device_enrollment::RegistrationReceipt;

pub type Result<T> = std::result::Result<T, LoginError>;

#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    #[error("invalid cloud origin or device configuration")]
    Configuration,
    #[error("native login is already being changed by another process")]
    Busy,
    #[error("local credential storage is unavailable")]
    Storage,
    #[error("local credential storage contains invalid data")]
    InvalidCredential,
    #[error("no saved login; complete browser authorization first")]
    LoginRequired,
    #[error("authorization was declined")]
    Declined,
    #[error("authorization timed out or was cancelled")]
    Cancelled,
    #[error("local authorization callback is unavailable")]
    Callback,
    #[error("cloud identity service is unavailable")]
    Network,
    #[error("cloud identity service returned an invalid response")]
    Protocol,
    #[error("cloud identity service rejected the request (HTTP {0})")]
    Rejected(u16),
    #[error("refresh outcome is unknown; authorize again rather than replaying the token")]
    RefreshUncertain,
}
