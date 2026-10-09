//! Native execution shared by the agent and standalone device executor.
//!
//! This crate has no model loop, account login, or approval middleware. The
//! cloud host authorizes a bound invocation before selecting its backend.

pub mod filesystem;
pub mod numeric;
pub mod output;
pub mod shell;
pub mod shell_contract;
