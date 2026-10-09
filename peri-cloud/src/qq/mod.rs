//! QQ official Bot adapter. Token caching, message event fields and REST paths
//! are adapted from Prism's gateway, independent of its other application code.

mod api;
mod card;
mod events;
mod webhook;
mod websocket;

pub use api::{QqAdapter, QqEndpoints, QqInteractionMode};
pub use events::{normalize, QqInput};
pub use webhook::webhook_router;

#[derive(Debug, thiserror::Error)]
pub enum QqError {
    #[error("QQ configuration is invalid")]
    Configuration,
    #[error("QQ platform connection is unavailable")]
    Connection,
    #[error("QQ event is invalid")]
    Event,
    #[error("QQ credentials or permission were rejected")]
    Authentication,
    #[error("QQ event admission failed")]
    Admission,
}

pub type QqResult<T> = Result<T, QqError>;
