//! Durable schedule management and dispatch. The host owns authentication,
//! task execution, memory, permissions and delivery. No agent runs in this crate.
mod domain;
mod engine;
mod store;
mod timing;

pub use domain::*;
pub use engine::{Clock, DrainReport, Scheduler, SystemClock, TaskDispatcher};
pub use store::{SchedulerStore, SqliteStore};
pub use timing::Timing;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid schedule: {0}")]
    Invalid(&'static str),
    #[error("schedule does not belong to this account")]
    Forbidden,
    #[error("schedule revision or idempotency key conflicts")]
    Conflict,
    #[error("schedule limit reached")]
    Limit,
    #[error("scheduler database version is unsupported")]
    Version,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Encoding(#[from] serde_json::Error),
}
pub type Result<T> = std::result::Result<T, Error>;
