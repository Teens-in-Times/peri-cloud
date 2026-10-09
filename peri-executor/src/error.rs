use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid executor request: {0}")]
    Invalid(String),
    #[error("executor object not found")]
    NotFound,
    #[error("invocation ID already belongs to a different request")]
    InvocationConflict,
    #[error("session workspace is already bound")]
    SessionConflict,
    #[error("bound workspace is unavailable or has changed")]
    WorkspaceChanged,
    #[error("session has an execution requiring recovery")]
    RecoveryRequired,
    #[error("executor is shutting down")]
    ShuttingDown,
    #[error("another executor owns this state directory")]
    AlreadyRunning,
    #[error("executor state storage failed")]
    Store(#[from] sqlx::Error),
    #[error("executor filesystem operation failed")]
    Io(#[from] std::io::Error),
    #[error("executor state encoding failed")]
    Encoding(#[from] serde_json::Error),
    #[error("executor worker failed")]
    Worker(#[from] tokio::task::JoinError),
}
