//! Shared commit capture and conditional rewind use the file tools' transaction lock.

pub(crate) use peri_tool_runtime::filesystem::transaction::{
    capture_committed_image, replace_if_unchanged, RewindCommitError, SENTINEL_REJECTION,
};
#[cfg(test)]
pub(crate) use peri_tool_runtime::filesystem::transaction::{
    gate_next_commit, with_target_lock, CommitGate,
};
