use sqlx::SqlitePool;

pub(crate) async fn migrate(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let (version,): (i64,) = sqlx::query_as("PRAGMA user_version")
        .fetch_one(&mut *tx)
        .await?;
    if version == 1 {
        for statement in [
            "CREATE TABLE principals (id TEXT PRIMARY KEY, login TEXT NOT NULL UNIQUE, display_name TEXT NOT NULL, password_hash TEXT NOT NULL)",
            "CREATE TABLE browser_sessions (token_hash TEXT PRIMARY KEY, principal TEXT NOT NULL REFERENCES principals(id), csrf_hash TEXT NOT NULL, expires_at INTEGER NOT NULL)",
            "CREATE TABLE identity_rates (rate_key TEXT PRIMARY KEY, window_started INTEGER NOT NULL, attempts INTEGER NOT NULL)",
            "CREATE TABLE native_codes (code_hash TEXT PRIMARY KEY, principal TEXT NOT NULL REFERENCES principals(id), device_id TEXT NOT NULL, authorization_json TEXT NOT NULL, expires_at INTEGER NOT NULL, used INTEGER NOT NULL DEFAULT 0)",
            "CREATE TABLE native_grants (id TEXT PRIMARY KEY, principal TEXT NOT NULL REFERENCES principals(id), device_id TEXT NOT NULL, access_hash TEXT NOT NULL UNIQUE, refresh_hash TEXT NOT NULL UNIQUE, access_expires INTEGER NOT NULL, refresh_expires INTEGER NOT NULL, revoked INTEGER NOT NULL DEFAULT 0)",
            "CREATE TABLE retired_refresh_tokens (token_hash TEXT PRIMARY KEY, grant_id TEXT NOT NULL REFERENCES native_grants(id))",
            "CREATE TABLE identity_devices (id TEXT PRIMARY KEY, principal TEXT NOT NULL REFERENCES principals(id), record_json TEXT NOT NULL)",
            "CREATE TABLE channel_identities (adapter_instance TEXT NOT NULL, external_user TEXT NOT NULL, principal TEXT NOT NULL REFERENCES principals(id), revoked INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(adapter_instance,external_user))",
            "CREATE TABLE pair_codes (code_hash TEXT PRIMARY KEY, principal TEXT NOT NULL REFERENCES principals(id), expires_at INTEGER NOT NULL, state TEXT NOT NULL, claim_id TEXT UNIQUE, identity_json TEXT)",
        ] { sqlx::query(statement).execute(&mut *tx).await?; }
        sqlx::query("PRAGMA user_version = 2")
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}
