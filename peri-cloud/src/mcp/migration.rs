use crate::state::{StateError, StateResult};
use sqlx::SqlitePool;

pub(crate) async fn migrate(pool: &SqlitePool) -> StateResult<()> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    if version >= 15 {
        return Ok(());
    }
    if version != 14 {
        return Err(StateError::Version);
    }
    let mut tx = pool.begin().await?;
    for sql in [
        "CREATE TABLE IF NOT EXISTS cloud_mcp_servers (\
         principal TEXT NOT NULL REFERENCES principals(id),\
         name TEXT NOT NULL,\
         url TEXT NOT NULL,\
         disabled INTEGER NOT NULL DEFAULT 0,\
         updated_at INTEGER NOT NULL,\
         PRIMARY KEY(principal,name))",
        "CREATE TABLE IF NOT EXISTS cloud_mcp_meta (\
         principal TEXT PRIMARY KEY REFERENCES principals(id),\
         revision INTEGER NOT NULL DEFAULT 0)",
        "PRAGMA user_version=15",
    ] {
        sqlx::query(sql).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}
