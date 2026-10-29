use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct Bookmark {
    pub id: Uuid,
    pub workspace: String,
    pub label: String,
    pub revision: i64,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum BookmarkCommand {
    Save {
        workspace: String,
        label: String,
    },
    Rename {
        id: Uuid,
        revision: i64,
        label: String,
    },
    Remove {
        id: Uuid,
        revision: i64,
    },
}

impl Gateway {
    pub(super) async fn device_bookmarks(
        &self,
        principal: Uuid,
        device: Uuid,
    ) -> GatewayResult<Vec<Bookmark>> {
        let rows = sqlx::query("SELECT id,workspace,label,revision FROM device_workspace_bookmarks WHERE principal=? AND device_id=? ORDER BY rowid LIMIT 32")
            .bind(principal.to_string()).bind(device.to_string()).fetch_all(&self.store.pool).await?;
        rows.into_iter()
            .map(|r| {
                Ok(Bookmark {
                    id: Uuid::parse_str(r.get("id")).map_err(|_| GatewayError::Invalid)?,
                    workspace: r.get("workspace"),
                    label: r.get("label"),
                    revision: r.get("revision"),
                })
            })
            .collect()
    }

    pub async fn browser_bookmark(
        &self,
        actor: &Authenticated,
        device: Uuid,
        command: BookmarkCommand,
    ) -> GatewayResult<Vec<Bookmark>> {
        self.identity.browser_scope(actor).await?;
        if !self
            .identity
            .devices(actor)
            .await?
            .iter()
            .any(|d| d.id == device && !d.revoked)
        {
            return Err(IdentityError::Forbidden.into());
        }
        let principal = actor.principal().id;
        if let BookmarkCommand::Save { label, .. } | BookmarkCommand::Rename { label, .. } =
            &command
        {
            let label = label.trim();
            if label.is_empty() || label.len() > 128 || label.chars().any(char::is_control) {
                return Err(GatewayError::Invalid);
            }
        }
        let command = match command {
            BookmarkCommand::Save { workspace, label } => {
                if workspace.trim().is_empty() || workspace.len() > 4096 {
                    return Err(GatewayError::Invalid);
                }
                let listing = self
                    .browse_browser_instance(actor, device, Some(workspace), 0)
                    .await?;
                if listing.path.is_empty() || listing.path.len() > 4096 {
                    return Err(GatewayError::Invalid);
                }
                BookmarkCommand::Save {
                    workspace: listing.path,
                    label,
                }
            }
            command => command,
        };
        let label = match &command {
            BookmarkCommand::Save { label, .. } | BookmarkCommand::Rename { label, .. } => {
                Some(label.trim())
            }
            BookmarkCommand::Remove { .. } => None,
        };
        let mut tx = self.store.pool.begin().await?;
        let authorized: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM identity_devices WHERE id=? AND principal=? AND json_extract(record_json,'$.revoked')=0")
            .bind(device.to_string()).bind(principal.to_string()).fetch_one(&mut *tx).await?;
        if authorized != 1 {
            return Err(IdentityError::Forbidden.into());
        }
        match &command {
            BookmarkCommand::Save { workspace, .. } => {
                let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM device_workspace_bookmarks WHERE principal=? AND device_id=? AND workspace=?")
                    .bind(principal.to_string()).bind(device.to_string()).bind(workspace).fetch_one(&mut *tx).await?;
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM device_workspace_bookmarks WHERE principal=? AND device_id=?")
                    .bind(principal.to_string()).bind(device.to_string()).fetch_one(&mut *tx).await?;
                if exists == 0 && count >= 32 {
                    return Err(GatewayError::BookmarkLimit);
                }
                sqlx::query("INSERT OR IGNORE INTO device_workspace_bookmarks(id,principal,device_id,workspace,label,revision) VALUES(?,?,?,?,?,1)")
                    .bind(Uuid::new_v4().to_string()).bind(principal.to_string()).bind(device.to_string()).bind(workspace).bind(label).execute(&mut *tx).await?;
            }
            BookmarkCommand::Rename { id, revision, .. } => {
                let changed = sqlx::query("UPDATE device_workspace_bookmarks SET label=?,revision=revision+1 WHERE id=? AND principal=? AND device_id=? AND revision=?")
                    .bind(label).bind(id.to_string()).bind(principal.to_string()).bind(device.to_string()).bind(revision).execute(&mut *tx).await?.rows_affected();
                if changed != 1 {
                    return Err(GatewayError::BookmarkConflict);
                }
            }
            BookmarkCommand::Remove { id, revision } => {
                let changed = sqlx::query("DELETE FROM device_workspace_bookmarks WHERE id=? AND principal=? AND device_id=? AND revision=?")
                    .bind(id.to_string()).bind(principal.to_string()).bind(device.to_string()).bind(revision).execute(&mut *tx).await?.rows_affected();
                if changed != 1 {
                    return Err(GatewayError::BookmarkConflict);
                }
            }
        }
        tx.commit().await?;
        if !self
            .identity
            .devices(actor)
            .await?
            .iter()
            .any(|d| d.id == device && !d.revoked)
        {
            return Err(IdentityError::Forbidden.into());
        }
        self.device_bookmarks(principal, device).await
    }
}

pub(crate) async fn migrate(pool: &sqlx::SqlitePool) -> crate::state::StateResult<()> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    if version >= 27 {
        return Ok(());
    }
    if version != 26 {
        return Err(crate::state::StateError::Version);
    }
    let mut tx = pool.begin().await?;
    sqlx::query("CREATE TABLE device_workspace_bookmarks(id TEXT PRIMARY KEY,principal TEXT NOT NULL REFERENCES principals(id),device_id TEXT NOT NULL REFERENCES identity_devices(id),workspace TEXT NOT NULL,label TEXT NOT NULL,revision INTEGER NOT NULL,UNIQUE(principal,device_id,workspace))").execute(&mut *tx).await?;
    sqlx::query("PRAGMA user_version=27")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
