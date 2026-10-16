use super::{AccessList, AccessPolicy, AccessRecord};
use crate::gateway::store::GatewayStore;
use crate::gateway::{ChannelRoute, GatewayError, GatewayResult};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

pub(crate) async fn migrate(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let (version,): (i64,) = sqlx::query_as("PRAGMA user_version")
        .fetch_one(&mut *tx)
        .await?;
    if version == 6 {
        for statement in [
            "CREATE TABLE gateway_access_policies (adapter TEXT PRIMARY KEY, revision INTEGER NOT NULL, policy_json TEXT NOT NULL)",
            "CREATE TABLE gateway_access_inputs (route_key TEXT PRIMARY KEY, principal TEXT NOT NULL REFERENCES principals(id), input_json TEXT NOT NULL, expires_at INTEGER NOT NULL)",
            "CREATE TABLE gateway_access_audit (id TEXT PRIMARY KEY, principal TEXT NOT NULL REFERENCES principals(id), adapter TEXT NOT NULL, revision INTEGER NOT NULL, policy_json TEXT NOT NULL, created_at INTEGER NOT NULL)",
            "PRAGMA user_version = 7",
        ] { sqlx::query(statement).execute(&mut *tx).await?; }
    }
    tx.commit().await?;
    Ok(())
}

#[derive(Serialize, Deserialize)]
pub(super) struct AccessInput {
    pub adapter: String,
    pub revision: u64,
    pub list: AccessList,
    pub expires_at: i64,
}

impl GatewayStore {
    pub(crate) async fn seed_access(
        &self,
        adapter: &str,
        policy: AccessPolicy,
    ) -> GatewayResult<()> {
        policy.validate()?;
        sqlx::query("INSERT OR IGNORE INTO gateway_access_policies(adapter,revision,policy_json) VALUES(?,0,?)")
            .bind(adapter).bind(serde_json::to_string(&policy)?).execute(&self.pool).await?;
        Ok(())
    }
    pub(crate) async fn access_policy(&self, adapter: &str) -> GatewayResult<AccessRecord> {
        let row =
            sqlx::query("SELECT revision,policy_json FROM gateway_access_policies WHERE adapter=?")
                .bind(adapter)
                .fetch_optional(&self.pool)
                .await?
                .ok_or(GatewayError::Invalid)?;
        let policy: AccessPolicy = serde_json::from_str(row.get("policy_json"))?;
        policy.validate()?;
        Ok(AccessRecord {
            revision: row.get::<i64, _>("revision") as u64,
            policy,
        })
    }
    pub(super) async fn save_access(
        &self,
        actor: Uuid,
        adapter: &str,
        expected: u64,
        policy: &AccessPolicy,
        now: i64,
    ) -> GatewayResult<()> {
        policy.validate()?;
        let json = serde_json::to_string(policy)?;
        let mut tx = self.pool.begin().await?;
        let changed=sqlx::query("UPDATE gateway_access_policies SET revision=revision+1,policy_json=? WHERE adapter=? AND revision=?")
            .bind(&json).bind(adapter).bind(expected as i64).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(crate::state::StateError::InvocationConflict.into());
        }
        sqlx::query("INSERT INTO gateway_access_audit(id,principal,adapter,revision,policy_json,created_at) VALUES(?,?,?,?,?,?)")
            .bind(Uuid::new_v4().to_string()).bind(actor.to_string()).bind(adapter).bind((expected+1) as i64).bind(json).bind(now).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
    pub(super) async fn set_access_input(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
        input: &AccessInput,
    ) -> GatewayResult<()> {
        sqlx::query("INSERT INTO gateway_access_inputs(route_key,principal,input_json,expires_at) VALUES(?,?,?,?) ON CONFLICT(route_key) DO UPDATE SET principal=excluded.principal,input_json=excluded.input_json,expires_at=excluded.expires_at")
            .bind(route.key()?).bind(principal.to_string()).bind(serde_json::to_string(input)?).bind(input.expires_at).execute(&self.pool).await?;
        Ok(())
    }
    pub(super) async fn access_input(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
    ) -> GatewayResult<Option<AccessInput>> {
        let row = sqlx::query(
            "SELECT input_json FROM gateway_access_inputs WHERE route_key=? AND principal=?",
        )
        .bind(route.key()?)
        .bind(principal.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| serde_json::from_str(r.get("input_json")).map_err(Into::into))
            .transpose()
    }
    pub(super) async fn clear_access_input(&self, route: &ChannelRoute) -> GatewayResult<()> {
        sqlx::query("DELETE FROM gateway_access_inputs WHERE route_key=?")
            .bind(route.key()?)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
