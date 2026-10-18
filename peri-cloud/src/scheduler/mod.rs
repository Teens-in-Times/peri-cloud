//! Cloud task/context adapters for the independent peri-scheduler kernel.
mod contact;
mod migration;
#[cfg(test)]
mod migration_test;
pub(crate) mod store;
#[cfg(test)]
mod tests;
pub(crate) mod tools;
pub use contact::ContactPolicy;
pub use peri_scheduler::{Error as ScheduleError, Result as ScheduleResult, Timing};
use serde::{Deserialize, Serialize};
pub(crate) use store::{Firing, ScheduleStore};
use uuid::Uuid;
pub const MAX_SCHEDULES_PER_USER: i64 = peri_scheduler::MAX_SCHEDULES_PER_OWNER;
pub const MAX_SCHEDULES: i64 = peri_scheduler::MAX_SCHEDULES;
pub const MISFIRE_GRACE_SECONDS: i64 = 300;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleMode {
    #[default]
    Conversation,
    Task,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Notify {
    #[default]
    Always,
    WhenUseful,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetBinding {
    Chat,
    Device,
    #[default]
    LegacyFrozenSession,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NewSchedule {
    #[serde(default)]
    pub mode: ScheduleMode,
    #[serde(default)]
    pub notify: Notify,
    pub request_id: Uuid,
    pub session_id: Uuid,
    pub title: String,
    pub prompt: String,
    pub timing: Timing,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleEdit {
    pub request_id: Uuid,
    pub revision: i64,
    pub title: String,
    pub prompt: String,
    pub timing: Timing,
    pub notify: Notify,
    pub enabled: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Schedule {
    pub mode: ScheduleMode,
    pub notify: Notify,
    pub id: Uuid,
    pub session_id: Uuid,
    pub device_id: Option<Uuid>,
    pub workspace: String,
    pub title: String,
    pub prompt: String,
    pub timing: Timing,
    pub enabled: bool,
    pub deleted: bool,
    pub revision: i64,
    pub next_fire: Option<i64>,
    pub created_at: i64,
    pub notification: String,
    #[serde(default)]
    pub binding: TargetBinding,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ScheduleRun {
    pub occurrence_id: Uuid,
    pub session_id: Option<Uuid>,
    pub delivery_state: String,
    pub schedule_id: Uuid,
    pub scheduled_at: i64,
    pub state: String,
    pub reason: Option<String>,
    pub turn_id: Option<Uuid>,
}
pub(crate) async fn migrate(pool: &sqlx::SqlitePool) -> crate::state::StateResult<()> {
    migration::migrate(pool).await
}
