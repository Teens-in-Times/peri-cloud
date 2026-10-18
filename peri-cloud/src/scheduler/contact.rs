use super::*;
use chrono::{TimeZone, Timelike};
use chrono_tz::Tz;
use sqlx::Row;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContactPolicy {
    pub revision: u64,
    pub enabled: bool,
    pub timezone: String,
    pub start_hour: u8,
    pub end_hour: u8,
    pub daily_limit: u32,
}
impl Default for ContactPolicy {
    fn default() -> Self {
        Self {
            revision: 0,
            enabled: true,
            timezone: "Asia/Shanghai".into(),
            start_hour: 8,
            end_hour: 22,
            daily_limit: 20,
        }
    }
}
impl ContactPolicy {
    pub fn allows(&self, now: i64, notify: Notify) -> bool {
        if !self.enabled {
            return false;
        }
        if notify == Notify::Always {
            return true;
        }
        let Ok(tz) = self.timezone.parse::<Tz>() else {
            return false;
        };
        let Some(time) = tz.timestamp_opt(now, 0).single() else {
            return false;
        };
        let hour = time.hour() as u8;
        if self.start_hour < self.end_hour {
            hour >= self.start_hour && hour < self.end_hour
        } else {
            hour >= self.start_hour || hour < self.end_hour
        }
    }
    fn validate(&self) -> ScheduleResult<()> {
        if self.timezone.len() > 64
            || self.timezone.parse::<Tz>().is_err()
            || self.start_hour > 23
            || self.end_hour > 24
            || self.start_hour == self.end_hour
            || self.daily_limit > 100
            || self.revision >= i64::MAX as u64
        {
            return Err(ScheduleError::Invalid("invalid proactive contact policy"));
        }
        Ok(())
    }
}
impl ScheduleStore {
    pub async fn contact_policy(&self, principal: Uuid) -> ScheduleResult<ContactPolicy> {
        let row = sqlx::query("SELECT record_json FROM scheduled_contact_policy WHERE owner=?")
            .bind(principal.to_string())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row
            .map(|r| serde_json::from_str(r.get("record_json")))
            .transpose()?
            .unwrap_or_default())
    }
    pub async fn save_contact_policy(
        &self,
        principal: Uuid,
        mut policy: ContactPolicy,
    ) -> ScheduleResult<ContactPolicy> {
        policy.validate()?;
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT record_json FROM scheduled_contact_policy WHERE owner=?")
            .bind(principal.to_string())
            .fetch_optional(&mut *tx)
            .await?;
        let current: ContactPolicy = row
            .map(|r| serde_json::from_str(r.get("record_json")))
            .transpose()?
            .unwrap_or_default();
        if current.revision != policy.revision {
            return Err(ScheduleError::Conflict);
        }
        policy.revision += 1;
        sqlx::query("INSERT INTO scheduled_contact_policy(owner,record_json) VALUES(?,?) ON CONFLICT(owner) DO UPDATE SET record_json=excluded.record_json")
            .bind(principal.to_string()).bind(serde_json::to_string(&policy)?).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(policy)
    }
}
