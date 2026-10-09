use std::collections::HashMap;

use peri_acp_types::device_executor::{JobSnapshot, SessionBinding};
use peri_acp_types::messages::{MessageContent, MessageId};
use peri_acp_types::permission::PermissionMode;
use peri_acp_types::store::{MessageFlags, PersistedPayload};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{ChatReply, TurnStatus};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrozenSession {
    pub session_id: Uuid,
    pub principal_id: Uuid,
    pub device_id: Uuid,
    pub binding: SessionBinding,
    pub system_prompt: String,
    pub context_window: u32,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub frozen: FrozenSession,
    pub permission_mode: u8,
    #[serde(with = "history_encoding")]
    pub history: Vec<PersistedPayload>,
    pub history_flags: HashMap<MessageId, MessageFlags>,
    /// None means executor state has not been confirmed after a turn/restart.
    pub unsettled_jobs: Option<Vec<JobSnapshot>>,
    pub revision: u64,
}

impl SessionState {
    pub fn permissions(&self) -> super::StateResult<PermissionMode> {
        if !matches!(self.permission_mode, 0 | 2 | 3 | 4) {
            return Err(super::StateError::PermissionMode);
        }
        Ok(PermissionMode::from(self.permission_mode))
    }

    pub fn execution_settled(&self) -> bool {
        matches!(&self.unsettled_jobs, Some(jobs) if jobs.is_empty())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    Queued,
    Running,
    Completed,
    Interrupted,
    Failed,
    RecoveryRequired,
}

impl From<TurnStatus> for TurnState {
    fn from(status: TurnStatus) -> Self {
        match status {
            TurnStatus::Completed => Self::Completed,
            TurnStatus::Interrupted => Self::Interrupted,
            TurnStatus::Failed => Self::Failed,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct TurnRecord {
    pub turn_id: Uuid,
    pub session_id: Uuid,
    pub request_key: String,
    pub prompt: MessageContent,
    pub permission_mode: u8,
    pub base_revision: u64,
    pub state: TurnState,
    pub replies: Vec<ChatReply>,
    pub cancel_requested: bool,
}

pub struct Admission {
    pub created: bool,
    pub turn: TurnRecord,
    pub session: SessionState,
}

mod history_encoding {
    use peri_acp_types::store::{
        deserialize_persisted_payload, serialize_persisted_payload, PersistedPayload,
    };
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(
        history: &[PersistedPayload],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let encoded: Vec<_> = history
            .iter()
            .map(|payload| {
                serialize_persisted_payload(payload)
                    .map_err(|_| serde::ser::Error::custom("invalid canonical history payload"))
            })
            .collect::<Result<_, S::Error>>()?;
        encoded.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<PersistedPayload>, D::Error> {
        Vec::<String>::deserialize(deserializer)?
            .iter()
            .map(|payload| {
                deserialize_persisted_payload(payload)
                    .map_err(|_| serde::de::Error::custom("invalid canonical history payload"))
            })
            .collect()
    }
}
