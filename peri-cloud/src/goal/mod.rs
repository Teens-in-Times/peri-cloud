//! Per-session goal state; the singleton slot is owned by the session binding.

mod controller;
mod migration;

pub(crate) use controller::CloudGoalController;
pub(crate) use migration::migrate;

#[cfg(test)]
mod tests;
