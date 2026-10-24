//! Session-scoped recovery copies. CloudJournal remains the turn/task authority.
mod storage;
mod tool;
pub(crate) use tool::ArchiveTool;

use crate::personal::PersonalScope;
use peri_acp_types::{
    messages::{ContentBlock, MessageId},
    session_resources::{SessionResourceError, SessionResourceErrorKind, SessionResourceResult},
    store::PersistedPayload,
};
use sqlx::{Row, SqliteConnection, SqlitePool};
use std::sync::Arc;

pub(crate) struct CloudArchive {
    scope: PersonalScope,
    generation: u64,
    epoch: u64,
    expires_at: Option<i64>,
}

impl CloudArchive {
    pub(crate) async fn open(scope: PersonalScope) -> SessionResourceResult<Arc<Self>> {
        let state = scope
            .journal
            .session(scope.principal, scope.session_id)
            .await
            .map_err(unavailable)?;
        let memory =
            crate::memory::MemoryStore::new(&scope.journal, scope.principal, scope.max_bytes);
        let epoch = memory.privacy_epoch().await.map_err(unavailable)?;
        let expires_at = memory.next_expiry().await.map_err(unavailable)?;
        let expires_at = if scope.private_memory {
            let (expected_epoch, expected_generation, expected_expiry) =
                crate::memory::frames::archive_fence(&scope).ok_or_else(forbidden)?;
            if expected_epoch != epoch || expected_generation != state.context_generation {
                return Err(forbidden());
            }
            expected_expiry.into_iter().chain(expires_at).min()
        } else {
            expires_at
        };
        Ok(Arc::new(Self {
            scope,
            generation: state.context_generation,
            epoch,
            expires_at,
        }))
    }

    async fn authorize(&self, conn: &mut SqliteConnection, id: &str) -> SessionResourceResult<()> {
        if id != self.scope.session_id.to_string()
            || self
                .expires_at
                .is_some_and(|at| at <= chrono::Utc::now().timestamp())
        {
            return Err(forbidden());
        }
        let row = sqlx::query("SELECT principal,COALESCE(json_extract(state_json,'$.context_generation'),0) AS generation,COALESCE((SELECT privacy_epoch FROM memory_accounts WHERE owner=cloud_sessions.principal),0) AS epoch FROM cloud_sessions WHERE id=?")
            .bind(id).fetch_optional(conn).await.map_err(unavailable)?.ok_or_else(forbidden)?;
        if row.get::<String, _>("principal") != self.scope.principal.to_string()
            || row.get::<i64, _>("generation") as u64 != self.generation
            || row.get::<i64, _>("epoch") as u64 != self.epoch
        {
            return Err(forbidden());
        }
        Ok(())
    }

    fn key(&self) -> String {
        format!(
            "{}:{}:{}",
            self.scope.session_id, self.generation, self.epoch
        )
    }
}

fn forbidden() -> SessionResourceError {
    SessionResourceError::new(SessionResourceErrorKind::ReadOnlyStore)
}
fn unavailable(error: impl std::fmt::Display) -> SessionResourceError {
    tracing::warn!(%error, "cloud archive unavailable");
    SessionResourceError::new(SessionResourceErrorKind::Unavailable {
        detail: "cloud archive unavailable".into(),
    })
}
fn invalid(detail: &str) -> SessionResourceError {
    SessionResourceError::new(SessionResourceErrorKind::InvalidInput {
        detail: detail.into(),
    })
}
fn not_found() -> SessionResourceError {
    SessionResourceError::new(SessionResourceErrorKind::NotFound)
}
fn message_id(value: String) -> SessionResourceResult<MessageId> {
    value
        .parse::<uuid::Uuid>()
        .map(Into::into)
        .map_err(unavailable)
}

// Reasoning and opaque provider blocks are never exposed through a recovery tool.
fn readable(payload: &PersistedPayload) -> bool {
    fn block(value: &ContentBlock) -> bool {
        match value {
            ContentBlock::Reasoning { .. } | ContentBlock::Unknown(_) => false,
            ContentBlock::ToolResult { content, .. } => content.iter().all(block),
            _ => true,
        }
    }
    payload
        .as_message()
        .is_none_or(|message| message.content_blocks().iter().all(block))
}

pub(crate) async fn migrate(pool: &SqlitePool) -> crate::state::StateResult<()> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    if version >= 25 {
        return Ok(());
    }
    if version != 24 {
        return Err(crate::state::StateError::Version);
    }
    let mut tx = pool.begin().await?;
    sqlx::query("CREATE TABLE cloud_archive_entries(scope TEXT NOT NULL,session_id TEXT NOT NULL REFERENCES cloud_sessions(id),message_id TEXT NOT NULL,ordinal INTEGER NOT NULL,canonical BLOB NOT NULL,projection_json TEXT,PRIMARY KEY(scope,message_id))").execute(&mut *tx).await?;
    sqlx::query("CREATE INDEX cloud_archive_order ON cloud_archive_entries(scope,ordinal)")
        .execute(&mut *tx)
        .await?;
    // Policy 2 had lossy previews without archive grants. The journal retained
    // originals: discard only obsolete rendering actions, never exclusion or
    // canonical history. Preserve Full's supersession evidence for rewind.
    let rows = sqlx::query("SELECT id,state_json FROM cloud_sessions")
        .fetch_all(&mut *tx)
        .await?;
    for row in rows {
        let mut state: serde_json::Value = serde_json::from_str(row.get("state_json"))?;
        let mut changed = false;
        if let Some(flags) = state
            .get_mut("history_flags")
            .and_then(|v| v.as_object_mut())
        {
            for flag in flags.values_mut() {
                if flag["projection"]["policy_version"] != 2 {
                    continue;
                }
                changed = true;
                flag["truncated"] = false.into();
                if flag["projection"]["superseded_by"].is_string() {
                    flag["projection"]["policy_version"] = 3.into();
                    flag["projection"]["entries"] = serde_json::json!([]);
                } else {
                    flag["projection"] = serde_json::Value::Null;
                }
            }
        }
        if changed {
            sqlx::query("UPDATE cloud_sessions SET state_json=? WHERE id=?")
                .bind(serde_json::to_string(&state)?)
                .bind(row.get::<String, _>("id"))
                .execute(&mut *tx)
                .await?;
        }
    }
    sqlx::query("PRAGMA user_version=25")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests;
