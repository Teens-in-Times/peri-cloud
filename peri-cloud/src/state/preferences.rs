//! Personal context is account-scoped, never mounted into an executor.
use super::{CloudJournal, StateError, StateResult};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UserPreferences {
    pub revision: u64,
    pub instructions: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct UserUsage {
    pub day: i64,
    pub turns: u32,
    pub model_requests: u32,
}

pub(crate) async fn migrate(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let (version,): (i64,) = sqlx::query_as("PRAGMA user_version")
        .fetch_one(&mut *tx)
        .await?;
    if version == 4 {
        for statement in [
            "CREATE TABLE account_roles (principal TEXT PRIMARY KEY REFERENCES principals(id), admin INTEGER NOT NULL DEFAULT 0, disabled INTEGER NOT NULL DEFAULT 0)",
            "INSERT INTO account_roles(principal,admin) SELECT id,1 FROM principals ORDER BY rowid LIMIT 1",
            "CREATE TABLE account_invites (id TEXT PRIMARY KEY, code_hash TEXT NOT NULL UNIQUE, issuer TEXT NOT NULL REFERENCES principals(id), expires_at INTEGER NOT NULL, used INTEGER NOT NULL DEFAULT 0)",
            "CREATE TABLE user_data (principal TEXT PRIMARY KEY REFERENCES principals(id), revision INTEGER NOT NULL, instructions TEXT NOT NULL, memory TEXT NOT NULL)",
            "CREATE TABLE user_usage (principal TEXT NOT NULL, day INTEGER NOT NULL, turns INTEGER NOT NULL DEFAULT 0, model_requests INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(principal,day))",
            "PRAGMA user_version = 5",
        ] { sqlx::query(statement).execute(&mut *tx).await?; }
    }
    let (version,): (i64,) = sqlx::query_as("PRAGMA user_version")
        .fetch_one(&mut *tx)
        .await?;
    if version == 5 {
        sqlx::query("CREATE TABLE user_models (principal TEXT PRIMARY KEY REFERENCES principals(id), provider TEXT NOT NULL, model TEXT NOT NULL, api_key TEXT NOT NULL)").execute(&mut *tx).await?;
        sqlx::query("PRAGMA user_version = 6")
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

impl CloudJournal {
    pub async fn preferences(&self, principal: Uuid) -> StateResult<UserPreferences> {
        let row =
            sqlx::query("SELECT revision,instructions FROM user_preferences WHERE principal=?")
                .bind(principal.to_string())
                .fetch_optional(&self.identity_pool())
                .await?;
        Ok(row
            .map(|r| UserPreferences {
                revision: r.get::<i64, _>("revision") as u64,
                instructions: r.get("instructions"),
            })
            .unwrap_or_default())
    }

    pub async fn save_preferences(
        &self,
        principal: Uuid,
        data: UserPreferences,
    ) -> StateResult<UserPreferences> {
        if data.instructions.len() > 4096 || data.revision >= i64::MAX as u64 {
            return Err(StateError::Limit);
        }
        let pool = self.identity_pool();
        let mut tx = pool.begin().await?;
        sqlx::query("INSERT OR IGNORE INTO user_preferences(principal,revision,instructions) VALUES(?,0,'')")
            .bind(principal.to_string()).execute(&mut *tx).await?;
        let changed = sqlx::query("UPDATE user_preferences SET revision=revision+1,instructions=? WHERE principal=? AND revision=?")
            .bind(&data.instructions).bind(principal.to_string()).bind(data.revision as i64)
            .execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(StateError::InvocationConflict);
        }
        tx.commit().await?;
        Ok(UserPreferences {
            revision: data.revision + 1,
            ..data
        })
    }

    pub async fn user_usage(&self, principal: Uuid) -> StateResult<UserUsage> {
        let day = usage_day();
        let row =
            sqlx::query("SELECT turns,model_requests FROM user_usage WHERE principal=? AND day=?")
                .bind(principal.to_string())
                .bind(day)
                .fetch_optional(&self.identity_pool())
                .await?;
        Ok(row
            .map(|r| UserUsage {
                day,
                turns: r.get::<i64, _>("turns") as u32,
                model_requests: r.get::<i64, _>("model_requests") as u32,
            })
            .unwrap_or(UserUsage {
                day,
                ..Default::default()
            }))
    }

    pub(crate) async fn charge_model_request(
        &self,
        principal: Uuid,
        limit: u32,
    ) -> StateResult<()> {
        let day = usage_day();
        let pool = self.identity_pool();
        let mut tx = pool.begin().await?;
        sqlx::query("INSERT OR IGNORE INTO user_usage(principal,day) VALUES(?,?)")
            .bind(principal.to_string())
            .bind(day)
            .execute(&mut *tx)
            .await?;
        let count = sqlx::query("UPDATE user_usage SET model_requests=model_requests+1 WHERE principal=? AND day=? AND model_requests<?")
            .bind(principal.to_string()).bind(day).bind(i64::from(limit)).execute(&mut *tx).await?.rows_affected();
        if count == 0 {
            return Err(StateError::Limit);
        }
        tx.commit().await?;
        Ok(())
    }
}

pub(crate) fn usage_day() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
        / 86400
}
