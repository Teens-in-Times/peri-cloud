//! Device execution without an agent loop or a second approval layer.

mod config;
mod directories;
mod environment;
mod error;
pub mod login;
mod native;
mod service;
mod shell;
mod store;
pub mod web;
mod workspace;

pub use error::{Error, Result};
pub use native::native_tool_catalog;
pub use service::Executor;
