//! Per-account MCP server configuration. No credentials are stored here.

use peri_middlewares::mcp::McpServerConfig;
use sqlx::SqlitePool;
use uuid::Uuid;

use super::McpConfigError;

const MAX_SERVERS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct McpServerRecord {
    pub name: String,
    pub url: String,
    pub disabled: bool,
}

#[derive(Clone)]
pub(crate) struct McpServerStore {
    pool: SqlitePool,
    principal: Uuid,
}

impl McpServerStore {
    pub(crate) fn new(pool: SqlitePool, principal: Uuid) -> Self {
        Self { pool, principal }
    }

    pub(crate) async fn revision(&self) -> Result<i64, McpConfigError> {
        let revision: Option<i64> =
            sqlx::query_scalar("SELECT revision FROM cloud_mcp_meta WHERE principal=?")
                .bind(self.principal.to_string())
                .fetch_optional(&self.pool)
                .await
                .map_err(unavailable)?;
        Ok(revision.unwrap_or(0))
    }

    pub(crate) async fn list(&self) -> Result<Vec<McpServerRecord>, McpConfigError> {
        let rows: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT name,url,disabled FROM cloud_mcp_servers WHERE principal=? ORDER BY name",
        )
        .bind(self.principal.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(unavailable)?;
        Ok(rows
            .into_iter()
            .map(|(name, url, disabled)| McpServerRecord {
                name,
                url,
                disabled: disabled != 0,
            })
            .collect())
    }

    pub(crate) async fn add(&self, name: &str, url: &str) -> Result<(), McpConfigError> {
        validate_name(name)?;
        let url = super::egress::validate(url).await?;
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        let exists: Option<(i64,)> =
            sqlx::query_as("SELECT 1 FROM cloud_mcp_servers WHERE principal=? AND name=?")
                .bind(self.principal.to_string())
                .bind(name)
                .fetch_optional(&mut *tx)
                .await
                .map_err(unavailable)?;
        if exists.is_none() {
            let (count,): (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM cloud_mcp_servers WHERE principal=?")
                    .bind(self.principal.to_string())
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(unavailable)?;
            if count as usize >= MAX_SERVERS {
                return Err(McpConfigError::Limit);
            }
        }
        sqlx::query(
            "INSERT INTO cloud_mcp_servers(principal,name,url,disabled,updated_at) \
             VALUES(?,?,?,0,?) \
             ON CONFLICT(principal,name) DO UPDATE SET \
             url=excluded.url, disabled=0, updated_at=excluded.updated_at",
        )
        .bind(self.principal.to_string())
        .bind(name)
        .bind(&url)
        .bind(chrono::Utc::now().timestamp())
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?;
        bump(&mut tx, self.principal).await?;
        tx.commit().await.map_err(unavailable)?;
        Ok(())
    }

    pub(crate) async fn remove(&self, name: &str) -> Result<(), McpConfigError> {
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        let deleted = sqlx::query("DELETE FROM cloud_mcp_servers WHERE principal=? AND name=?")
            .bind(self.principal.to_string())
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?;
        if deleted.rows_affected() == 0 {
            return Err(McpConfigError::NotFound);
        }
        bump(&mut tx, self.principal).await?;
        tx.commit().await.map_err(unavailable)?;
        Ok(())
    }

    pub(crate) async fn set_disabled(
        &self,
        name: &str,
        disabled: bool,
    ) -> Result<(), McpConfigError> {
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        let updated = sqlx::query(
            "UPDATE cloud_mcp_servers SET disabled=?, updated_at=? WHERE principal=? AND name=?",
        )
        .bind(if disabled { 1_i64 } else { 0_i64 })
        .bind(chrono::Utc::now().timestamp())
        .bind(self.principal.to_string())
        .bind(name)
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?;
        if updated.rows_affected() == 0 {
            return Err(McpConfigError::NotFound);
        }
        bump(&mut tx, self.principal).await?;
        tx.commit().await.map_err(unavailable)?;
        Ok(())
    }

    pub(crate) async fn enabled(&self) -> Result<Vec<McpServerRecord>, McpConfigError> {
        Ok(self
            .list()
            .await?
            .into_iter()
            .filter(|server| !server.disabled)
            .collect())
    }
}

pub(crate) fn config(record: &McpServerRecord) -> McpServerConfig {
    McpServerConfig {
        command: None,
        args: None,
        env: None,
        url: Some(record.url.clone()),
        headers: None,
        oauth: None,
        disabled: None,
        subscriptions: None,
        system_mcp: None,
        system_mcp_tools: None,
        system_mcp_timeout: None,
        source: None,
    }
}

fn validate_name(name: &str) -> Result<(), McpConfigError> {
    if name.is_empty()
        || name.len() > 32
        || name.contains("__")
        || !name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
    {
        return Err(McpConfigError::Name);
    }
    if name == "workspace" || name.starts_with("mcp__") {
        return Err(McpConfigError::Reserved(name.to_owned()));
    }
    Ok(())
}

async fn bump(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    principal: Uuid,
) -> Result<(), McpConfigError> {
    sqlx::query(
        "INSERT INTO cloud_mcp_meta(principal,revision) VALUES(?,1) \
         ON CONFLICT(principal) DO UPDATE SET revision=revision+1",
    )
    .bind(principal.to_string())
    .execute(&mut **tx)
    .await
    .map_err(unavailable)?;
    Ok(())
}

fn unavailable(error: sqlx::Error) -> McpConfigError {
    tracing::warn!(%error, "MCP server store failed");
    McpConfigError::Unavailable
}
