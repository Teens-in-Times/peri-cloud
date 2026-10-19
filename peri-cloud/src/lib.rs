//! Personal cloud agent host, independent of TUI and message channel.

pub mod aggregation;
mod assembly;
pub mod commands;
pub mod context;
mod core;
pub mod gateway;
mod goal;
pub mod host;
pub mod identity;
pub mod limits;
mod mcp;
pub mod memory;
pub mod models;
pub mod onebot;
mod personal;
pub mod portal;
pub mod qq;
mod reply;
mod runtime;
pub mod scheduler;
pub mod state;
mod tasks;
mod workspace;

pub use core::{CloudAgent, CloudTurnRequest, CloudTurnResult, Error, TurnStatus};
pub use reply::ChatReply;
pub use runtime::{CloudRuntime, RuntimeError, RuntimeStatus, ShutdownReport, SubmitTurn};
