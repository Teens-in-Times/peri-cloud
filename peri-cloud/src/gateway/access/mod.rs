//! Shared transport access rules. This gate does not replace account binding,
//! session ownership or Peri tool permission policy.
mod browser;
mod menus;
mod store;

use super::{ChannelRoute, GatewayError, GatewayResult};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationScope {
    Private,
    Group(String),
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccessMode {
    #[default]
    Allowlist,
    Blocklist,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct AccessPolicy {
    pub mode: AccessMode,
    pub allowed_users: BTreeSet<String>,
    pub allowed_groups: BTreeSet<String>,
    pub blocked_users: BTreeSet<String>,
    pub blocked_groups: BTreeSet<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccessRecord {
    pub revision: u64,
    pub policy: AccessPolicy,
}

#[derive(Clone, Debug, Serialize)]
pub struct GatewayAccess {
    pub adapter: String,
    pub record: AccessRecord,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessList {
    AllowedUsers,
    AllowedGroups,
    BlockedUsers,
    BlockedGroups,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AccessChange {
    Mode { mode: AccessMode },
    Add { list: AccessList, id: String },
    Remove { list: AccessList, id: String },
}

impl AccessPolicy {
    /// Explicitly retain an existing gateway's pre-policy behavior.
    pub fn open() -> Self {
        Self {
            mode: AccessMode::Blocklist,
            ..Self::default()
        }
    }
    pub fn validate(&self) -> GatewayResult<()> {
        for list in [
            &self.allowed_users,
            &self.allowed_groups,
            &self.blocked_users,
            &self.blocked_groups,
        ] {
            if list.len() > 128 || list.iter().any(|id| !valid_subject(id)) {
                return Err(GatewayError::Invalid);
            }
        }
        Ok(())
    }
    pub fn allows(&self, route: &ChannelRoute, scope: &ConversationScope) -> bool {
        let user = &route.identity.external_user_id;
        if self.blocked_users.contains(user) {
            return false;
        }
        if let ConversationScope::Group(group) = scope {
            if self.blocked_groups.contains(group) {
                return false;
            }
        }
        if self.mode == AccessMode::Blocklist {
            return true;
        }
        self.allowed_users.contains(user)
            && match scope {
                ConversationScope::Private => true,
                ConversationScope::Group(group) => self.allowed_groups.contains(group),
                ConversationScope::Unknown => false,
            }
    }
    pub(super) fn list(&self, kind: AccessList) -> &BTreeSet<String> {
        match kind {
            AccessList::AllowedUsers => &self.allowed_users,
            AccessList::AllowedGroups => &self.allowed_groups,
            AccessList::BlockedUsers => &self.blocked_users,
            AccessList::BlockedGroups => &self.blocked_groups,
        }
    }
    fn list_mut(&mut self, kind: AccessList) -> &mut BTreeSet<String> {
        match kind {
            AccessList::AllowedUsers => &mut self.allowed_users,
            AccessList::AllowedGroups => &mut self.allowed_groups,
            AccessList::BlockedUsers => &mut self.blocked_users,
            AccessList::BlockedGroups => &mut self.blocked_groups,
        }
    }
    pub(super) fn changed(&self, change: &AccessChange) -> GatewayResult<Self> {
        let mut next = self.clone();
        match change {
            AccessChange::Mode { mode } => next.mode = *mode,
            AccessChange::Add { list, id } => {
                if !valid_subject(id) {
                    return Err(GatewayError::Invalid);
                }
                next.list_mut(*list).insert(id.clone());
            }
            AccessChange::Remove { list, id } => {
                next.list_mut(*list).remove(id);
            }
        }
        next.validate()?;
        Ok(next)
    }
}

pub fn valid_subject(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && !id.chars().any(|ch| ch.is_control() || ch.is_whitespace())
}

pub(crate) use store::migrate;

#[cfg(test)]
#[path = "mod_test.rs"]
mod tests;
