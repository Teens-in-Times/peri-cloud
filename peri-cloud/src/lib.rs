//! Personal cloud agent host, independent of TUI and message channel.

mod assembly;
mod core;
pub mod gateway;
pub mod host;
pub mod identity;
pub mod portal;
pub mod qq;
mod reply;
mod runtime;
pub mod state;

pub use core::{CloudAgent, CloudTurnRequest, CloudTurnResult, Error, TurnStatus};
pub use reply::ChatReply;
pub use runtime::{CloudRuntime, RuntimeError, RuntimeStatus, ShutdownReport, SubmitTurn};
