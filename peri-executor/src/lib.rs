//! Device execution without an agent loop or a second approval layer.

mod config;
mod error;
pub mod login;
mod native;
mod service;
mod shell;
mod store;
pub mod web;
mod workspace;

pub use error::{Error, Result};
pub use service::Executor;
