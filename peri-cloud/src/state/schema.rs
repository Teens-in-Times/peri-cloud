//! Production schema 9: device connection leases and persisted Web chat.
//!
//! The local standalone distribution also uses `web_messages` and
//! `device_connections`, but at `user_version = 6`. Opening such a database
//! here would silently skip its tables, so every check runs before the first
//! write and refuses the foreign lineage instead of migrating it.

use sqlx::SqlitePool;

use super::{StateError, StateResult};

/// `PERC`, stored in SQLite's application_id once schema 9 is applied.
pub(crate) const APPLICATION_ID: i64 = 0x5045_5243;
const _: () = assert!(APPLICATION_ID == 1_346_720_323);

async fn has_table(pool: &SqlitePool, name: &str) -> StateResult<bool> {
    let (count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?")
            .bind(name)
            .fetch_one(pool)
            .await?;
    Ok(count != 0)
}

/// Refuse databases created by the standalone schema lineage before any
/// migration can add production tables to them.
pub(crate) async fn reject_foreign(pool: &SqlitePool, version: i64) -> StateResult<()> {
    let (application_id,): (i64,) = sqlx::query_as("PRAGMA application_id")
        .fetch_one(pool)
        .await?;
    if application_id != 0 && application_id != APPLICATION_ID {
        return Err(StateError::ForeignSchema);
    }
    if version >= 9 && application_id != APPLICATION_ID {
        return Err(StateError::ForeignSchema);
    }
    if version == 5
        && has_table(pool, "gateway_queue").await?
        && !has_table(pool, "account_roles").await?
    {
        return Err(StateError::ForeignSchema);
    }
    if (6..=16).contains(&version) {
        let standalone_markers =
            has_table(pool, "web_messages").await? || has_table(pool, "device_connections").await?;
        if standalone_markers && !has_table(pool, "user_models").await? {
            return Err(StateError::ForeignSchema);
        }
    }
    if version >= 9
        && (!has_table(pool, "user_models").await?
            || !has_table(pool, "device_connections").await?
            || !has_table(pool, "web_messages").await?)
    {
        return Err(StateError::ForeignSchema);
    }
    if version == 10
        && (!has_table(pool, "cloud_schedules").await?
            || !has_table(pool, "cloud_schedule_runs").await?)
    {
        return Err(StateError::ForeignSchema);
    }
    if version >= 11
        && (!has_table(pool, "scheduled_tasks").await?
            || !has_table(pool, "scheduled_admissions").await?
            || !has_table(pool, "scheduler_metadata").await?)
    {
        return Err(StateError::ForeignSchema);
    }
    if version >= 12
        && (!has_table(pool, "memory_items").await?
            || !has_table(pool, "memory_fts").await?
            || !has_table(pool, "user_preferences").await?
            || !has_table(pool, "memory_accounts").await?
            || !has_table(pool, "memory_audit").await?
            || !has_table(pool, "memory_session_audience").await?)
    {
        return Err(StateError::ForeignSchema);
    }
    if version >= 14 && !has_table(pool, "cloud_goals").await? {
        return Err(StateError::ForeignSchema);
    }
    if version >= 15
        && (!has_table(pool, "cloud_mcp_servers").await?
            || !has_table(pool, "cloud_mcp_meta").await?)
    {
        return Err(StateError::ForeignSchema);
    }
    if version >= 16
        && (!has_table(pool, "gateway_queue").await?
            || !has_table(pool, "agent_tasks").await?
            || !has_table(pool, "agent_task_messages").await?
            || !has_table(pool, "agent_task_trace").await?
            || !has_table(pool, "agent_device_leases").await?
            || !has_table(pool, "conversation_projections").await?)
    {
        return Err(StateError::ForeignSchema);
    }
    if version >= 17
        && (!has_table(pool, "agent_execution_sessions").await?
            || !has_table(pool, "agent_execution_windows").await?)
    {
        return Err(StateError::ForeignSchema);
    }
    if version >= 18
        && (!has_table(pool, "agent_task_checkpoints").await?
            || !has_table(pool, "agent_task_operations").await?)
    {
        return Err(StateError::ForeignSchema);
    }
    if version >= 19 && !has_table(pool, "agent_task_runs").await? {
        return Err(StateError::ForeignSchema);
    }
    if version >= 20
        && (!has_table(pool, "agent_task_approvals").await?
            || !has_table(pool, "agent_task_instruction_reads").await?)
    {
        return Err(StateError::ForeignSchema);
    }
    Ok(())
}

/// Apply schema 9 once, after every production migration through schema 8.
pub(crate) async fn migrate(pool: &SqlitePool) -> StateResult<()> {
    let (version,): (i64,) = sqlx::query_as("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    if version >= 9 {
        return Ok(());
    }
    if version != 8 {
        return Err(StateError::Version);
    }
    let mut tx = pool.begin().await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS device_connections (\
         id TEXT PRIMARY KEY,\
         device_id TEXT NOT NULL UNIQUE REFERENCES identity_devices(id),\
         principal TEXT NOT NULL REFERENCES principals(id),\
         endpoint TEXT NOT NULL,\
         token_path TEXT NOT NULL,\
         lease_expires_at INTEGER NOT NULL,\
         updated_at INTEGER NOT NULL)",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS device_connections_lease \
         ON device_connections(device_id, lease_expires_at)",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS web_messages (\
         seq INTEGER PRIMARY KEY AUTOINCREMENT,\
         route_key TEXT NOT NULL,\
         principal TEXT NOT NULL,\
         event_key TEXT,\
         delivery_id TEXT UNIQUE,\
         role TEXT NOT NULL,\
         text TEXT NOT NULL,\
         card_json TEXT,\
         state TEXT NOT NULL,\
         created_at INTEGER NOT NULL)",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS web_messages_event \
         ON web_messages(event_key) WHERE event_key IS NOT NULL",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS web_messages_route ON web_messages(route_key, seq)")
        .execute(&mut *tx)
        .await?;
    sqlx::query("PRAGMA user_version = 9")
        .execute(&mut *tx)
        .await?;
    // Keep the literal in sync with APPLICATION_ID (0x50455243).
    sqlx::query("PRAGMA application_id = 1346720323")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
