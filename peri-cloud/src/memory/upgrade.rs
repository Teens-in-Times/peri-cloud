use crate::state::StateResult;
use sqlx::SqlitePool;

pub(super) async fn migrate(pool: &SqlitePool) -> StateResult<()> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    if version >= 13 {
        return Ok(());
    }
    if version != 12 {
        return Err(crate::state::StateError::Version);
    }
    let mut tx = pool.begin().await?;
    for sql in [
        "ALTER TABLE memory_items ADD COLUMN scope TEXT NOT NULL DEFAULT 'user'",
        "CREATE INDEX memory_scope ON memory_items(owner,scope,deleted,updated_at,id)",
        "ALTER TABLE memory_accounts ADD COLUMN privacy_epoch INTEGER NOT NULL DEFAULT 0",
        "PRAGMA user_version=13",
    ] {
        sqlx::query(sql).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}
