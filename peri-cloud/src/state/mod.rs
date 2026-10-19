//! Durable cloud session and turn journal. No transport credentials live here.

pub(crate) mod chat_projection;
mod core_migration;
mod journal;
mod scheduled;
mod schema;
pub(crate) use scheduled::ScheduledAdmission;
pub(crate) mod preferences;
mod types;

pub use journal::CloudJournal;
pub use preferences::{UserPreferences, UserUsage};
pub use types::{Admission, FrozenSession, SessionState, TurnRecord, TurnState};

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error(transparent)]
    Scheduler(#[from] peri_scheduler::Error),
    #[error("account resource limit reached")]
    Limit,
    #[error("cloud state is unavailable")]
    Database(#[from] sqlx::Error),
    #[error("cloud state cannot be opened")]
    Io(#[from] std::io::Error),
    #[error("cloud state contains an invalid record")]
    Encoding(#[from] serde_json::Error),
    #[error("cloud state is owned by another process")]
    AlreadyOwned,
    #[error("cloud state version is unsupported")]
    Version,
    #[error("database belongs to a different Peri Cloud schema lineage")]
    ForeignSchema,
    #[error("cloud session or turn was not found")]
    NotFound,
    #[error("cloud principal does not own this session")]
    Forbidden,
    #[error("cloud session has a different frozen device binding")]
    BindingConflict,
    #[error("request identity was reused with different content")]
    InvocationConflict,
    #[error("cloud session still has active or unconfirmed execution")]
    Busy,
    #[error("cloud turn transition conflicts with its durable state")]
    TransitionConflict,
    #[error("cloud permission mode is unsupported")]
    PermissionMode,
}

pub type StateResult<T> = Result<T, StateError>;
