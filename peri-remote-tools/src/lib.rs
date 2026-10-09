//! Native tool adapters for an authenticated, frozen device session.

mod client;
mod session;
mod tool;

pub use client::{DeviceClient, Error, Result};
pub use session::RemoteSession;
