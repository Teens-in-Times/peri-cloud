use crate::state::{StateError, StateResult};
use sqlx::SqlitePool;

pub(crate) async fn migrate(pool: &SqlitePool) -> StateResult<()> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    if version >= 14 {
        return Ok(());
    }
    if version != 13 {
        return Err(StateError::Version);
    }
    let mut tx = pool.begin().await?;
    for sql in [
        "CREATE TABLE IF NOT EXISTS cloud_goals (\
         session_id TEXT PRIMARY KEY REFERENCES cloud_sessions(id),\
         principal TEXT NOT NULL REFERENCES principals(id),\
         goal_json TEXT NOT NULL,\
         updated_at INTEGER NOT NULL)",
        "CREATE INDEX IF NOT EXISTS cloud_goals_principal ON cloud_goals(principal)",
        "PRAGMA user_version=14",
    ] {
        sqlx::query(sql).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}
