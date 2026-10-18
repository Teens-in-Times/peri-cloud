use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::Timing;

pub const MAX_SCHEDULES_PER_OWNER: i64 = 20;
pub const MAX_SCHEDULES: i64 = 1000;
pub const MAX_BATCH: usize = 32;

/// Construct only after the host has authenticated the resource owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OwnerScope(pub Uuid);

/// Opaque immutable task version. This crate never loads its contents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRef {
    pub id: Uuid,
    pub revision: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Misfire {
    #[default]
    Skip,
    Latest,
    CatchUp {
        limit: u8,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub start_grace_seconds: i64,
    pub misfire: Misfire,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            start_grace_seconds: 300,
            misfire: Misfire::Skip,
        }
    }
}
impl Policy {
    pub(crate) fn validate(&self) -> crate::Result<()> {
        if !(1..=604800).contains(&self.start_grace_seconds)
            || matches!(self.misfire, Misfire::CatchUp { limit } if limit == 0 || usize::from(limit) > MAX_BATCH)
        {
            return Err(crate::Error::Invalid(
                "invalid grace period or catch-up limit",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleSpec {
    pub id: Uuid,
    pub task: TaskRef,
    pub timing: Timing,
    #[serde(default)]
    pub policy: Policy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub owner: OwnerScope,
    pub spec: ScheduleSpec,
    pub revision: i64,
    pub enabled: bool,
    #[serde(default)]
    pub paused: bool,
    pub deleted: bool,
    pub next_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScheduleCommand {
    Create {
        spec: ScheduleSpec,
    },
    Update {
        id: Uuid,
        expected_revision: i64,
        timing: Option<Timing>,
        task: Option<TaskRef>,
        enabled: Option<bool>,
    },
    Remove {
        id: Uuid,
        expected_revision: i64,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandEnvelope {
    pub command_id: Uuid,
    pub command: ScheduleCommand,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchState {
    Pending,
    Dispatching,
    Deferred,
    Accepted,
    Rejected,
    Skipped,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Occurrence {
    pub id: Uuid,
    pub owner: OwnerScope,
    pub schedule_id: Uuid,
    pub planned_at: i64,
    pub task: TaskRef,
    pub start_deadline: i64,
    pub state: DispatchState,
    pub next_attempt: i64,
    pub reason: Option<String>,
    pub run_ref: Option<String>,
}
impl Occurrence {
    pub fn stable_id(schedule: Uuid, planned_at: i64) -> Uuid {
        let hash = Sha256::digest(format!("peri:occurrence:{schedule}:{planned_at}").as_bytes());
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&hash[..16]);
        Uuid::from_bytes(bytes)
    }
    /// Stable across the legacy in-cloud implementation and crate migration.
    pub fn request_key(&self) -> String {
        format!("schedule:{}:{}", self.schedule_id, self.planned_at)
    }
}

#[derive(Clone, Debug)]
pub enum SubmitOutcome {
    Accepted { run_ref: String },
    Deferred { until: i64, reason: String },
    Rejected { reason: String },
    Unknown,
}
#[derive(Clone, Debug)]
pub enum LookupOutcome {
    Accepted {
        run_ref: String,
    },
    /// Requires authoritative proof; a transport error or ordinary 404 is insufficient.
    DefinitelyNotAccepted,
    Unresolved,
}

#[derive(Clone, Debug)]
pub enum ScheduleQuery {
    Plans { offset: u32, limit: u16 },
    Plan { id: Uuid },
    Occurrences { id: Uuid, offset: u32, limit: u16 },
}
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", content = "items", rename_all = "snake_case")]
pub enum QueryPage {
    Plans(Vec<Plan>),
    Plan(Plan),
    Occurrences(Vec<Occurrence>),
}
