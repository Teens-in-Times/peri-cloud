use super::*;
use sqlx::{Row, SqlitePool};

pub(crate) async fn migrate(pool: &SqlitePool) -> crate::state::StateResult<()> {
    let (version,): (i64,) = sqlx::query_as("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    if version >= 12 {
        return super::upgrade::migrate(pool).await;
    }
    if version != 11 {
        return Err(crate::state::StateError::Version);
    }
    let mut tx = pool.begin().await?;
    for sql in [
        "CREATE TABLE user_preferences (principal TEXT PRIMARY KEY REFERENCES principals(id),revision INTEGER NOT NULL, instructions TEXT NOT NULL)",
        "INSERT INTO user_preferences SELECT principal,revision,instructions FROM user_data",
        "CREATE TABLE memory_accounts (owner TEXT PRIMARY KEY REFERENCES principals(id),generation INTEGER NOT NULL DEFAULT 0)",
        "CREATE TABLE memory_session_audience (session_id TEXT PRIMARY KEY REFERENCES cloud_sessions(id),private_memory INTEGER NOT NULL)",
        "CREATE TABLE memory_items (id TEXT PRIMARY KEY,owner TEXT NOT NULL REFERENCES principals(id),revision INTEGER NOT NULL,content TEXT NOT NULL,kind TEXT NOT NULL,pinned INTEGER NOT NULL,valid_until INTEGER,source_json TEXT NOT NULL,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,deleted INTEGER NOT NULL DEFAULT 0,create_hash TEXT NOT NULL,search_text TEXT NOT NULL)",
        "CREATE INDEX memory_owner ON memory_items(owner,deleted,updated_at,id)",
        "CREATE VIRTUAL TABLE memory_fts USING fts5(item_id UNINDEXED,owner UNINDEXED,terms,tokenize='unicode61')",
        "CREATE TABLE memory_audit (seq INTEGER PRIMARY KEY AUTOINCREMENT,owner TEXT NOT NULL,item_id TEXT NOT NULL,revision INTEGER NOT NULL,action TEXT NOT NULL,source_json TEXT NOT NULL,created_at INTEGER NOT NULL)",
    ] { sqlx::query(sql).execute(&mut *tx).await?; }
    let rows = sqlx::query("SELECT principal,memory FROM user_data WHERE memory<>''")
        .fetch_all(&mut *tx)
        .await?;
    for row in rows {
        let owner: String = row.get("principal");
        let content: String = row.get("memory");
        let id = Uuid::new_v4().to_string();
        // Preserve one original block. No invented per-fact provenance or date.
        let source = serde_json::to_string(&MemorySource::entry("legacy_import"))?;
        let pinned =
            content.chars().count() <= SNAPSHOT_CHARS && content.len() < SNAPSHOT_BYTES / 2;
        sqlx::query("INSERT INTO memory_accounts(owner,generation) VALUES(?,1)")
            .bind(&owner)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO memory_items(id,owner,revision,content,kind,pinned,source_json,created_at,updated_at,create_hash,search_text) VALUES(?,?,1,?,'fact',?,?,0,0,'legacy',?)")
            .bind(&id).bind(&owner).bind(&content).bind(pinned).bind(&source).bind(content.to_lowercase()).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO memory_fts(item_id,owner,terms) VALUES(?,?,?)")
            .bind(&id)
            .bind(&owner)
            .bind(search::terms(&content).join(" "))
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO memory_audit(owner,item_id,revision,action,source_json,created_at) VALUES(?,?,1,'import',?,0)").bind(owner).bind(id).bind(source).execute(&mut *tx).await?;
    }
    sqlx::query("DROP TABLE user_data")
        .execute(&mut *tx)
        .await?;
    sqlx::query("PRAGMA user_version=12")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    super::upgrade::migrate(pool).await
}
