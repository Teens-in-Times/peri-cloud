//! Deployment-owned device connection registry. The transport token is a
//! device-scoped secret reported over the authenticated native channel; it is
//! kept in a private file and is never exposed to the browser or the model.

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use super::{DeviceRecord, IdentityError, IdentityResult};
use peri_acp_types::device_enrollment::{ConnectionReceipt, ConnectionReport};

const MIN_LEASE: u32 = 30;
const MAX_LEASE: u32 = 600;

#[derive(Clone)]
pub(crate) struct ConnectionStore {
    pool: SqlitePool,
    token_dir: PathBuf,
}

pub(crate) struct ActiveConnection {
    pub endpoint: SocketAddr,
    pub token_path: PathBuf,
}

impl ConnectionStore {
    pub(crate) fn new(pool: SqlitePool, state_dir: &Path) -> Self {
        Self {
            pool,
            token_dir: state_dir.join("connections"),
        }
    }

    /// Store only after the device record is known to belong to this account
    /// and is not revoked. A short lease makes a stale reverse-tunnel endpoint
    /// stop being used even if the device disappears without a report.
    pub(crate) async fn publish(
        &self,
        principal: Uuid,
        report: ConnectionReport,
        now: i64,
    ) -> IdentityResult<ConnectionReceipt> {
        let endpoint: SocketAddr = report
            .endpoint
            .trim()
            .parse()
            .map_err(|_| IdentityError::Invalid)?;
        if !endpoint.ip().is_loopback() || endpoint.port() == 0 {
            return Err(IdentityError::Invalid);
        }
        if !(MIN_LEASE..=MAX_LEASE).contains(&report.lease_seconds) {
            return Err(IdentityError::Invalid);
        }
        let token = report.transport_token.trim();
        if !(64..=8192).contains(&token.len())
            || token
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control())
        {
            return Err(IdentityError::Invalid);
        }
        let mut tx = self.pool.begin().await?;
        let row =
            sqlx::query("SELECT record_json FROM identity_devices WHERE id=? AND principal=?")
                .bind(report.device_id.to_string())
                .bind(principal.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(IdentityError::Forbidden)?;
        let mut record: DeviceRecord = serde_json::from_str(row.get("record_json"))?;
        if record.revoked || record.is_local {
            return Err(IdentityError::Forbidden);
        }
        let existing: Option<String> = sqlx::query_scalar(
            "SELECT id FROM device_connections WHERE device_id=? AND principal=?",
        )
        .bind(report.device_id.to_string())
        .bind(principal.to_string())
        .fetch_optional(&mut *tx)
        .await?;
        let connection_id = existing
            .and_then(|value| Uuid::parse_str(&value).ok())
            .unwrap_or_else(Uuid::new_v4);
        let token_path = self.token_dir.join(format!("{}.token", report.device_id));
        let expires = now.saturating_add(i64::from(report.lease_seconds));
        sqlx::query("INSERT INTO device_connections(id,device_id,principal,endpoint,token_path,lease_expires_at,updated_at) VALUES(?,?,?,?,?,?,?) ON CONFLICT(device_id) DO UPDATE SET id=excluded.id,principal=excluded.principal,endpoint=excluded.endpoint,token_path=excluded.token_path,lease_expires_at=excluded.lease_expires_at,updated_at=excluded.updated_at")
            .bind(connection_id.to_string())
            .bind(report.device_id.to_string())
            .bind(principal.to_string())
            .bind(endpoint.to_string())
            .bind(token_path.to_string_lossy().as_ref())
            .bind(expires)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        record.connection_id = Some(connection_id);
        sqlx::query("UPDATE identity_devices SET record_json=? WHERE id=? AND principal=?")
            .bind(serde_json::to_string(&record)?)
            .bind(report.device_id.to_string())
            .bind(principal.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        write_token(self.token_dir.clone(), token_path, token.to_owned()).await?;
        Ok(ConnectionReceipt {
            connection_id,
            lease_expires_at: expires,
        })
    }

    pub(crate) async fn active(
        &self,
        device_id: Uuid,
        now: i64,
    ) -> IdentityResult<Option<ActiveConnection>> {
        let row = sqlx::query(
            "SELECT endpoint,token_path FROM device_connections WHERE device_id=? AND lease_expires_at>?",
        )
        .bind(device_id.to_string())
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok(ActiveConnection {
                endpoint: row
                    .get::<String, _>("endpoint")
                    .parse()
                    .map_err(|_| IdentityError::Invalid)?,
                token_path: PathBuf::from(row.get::<String, _>("token_path")),
            })
        })
        .transpose()
    }

    pub(crate) async fn exists(&self, device_id: Uuid) -> IdentityResult<bool> {
        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM device_connections WHERE device_id=?")
                .bind(device_id.to_string())
                .fetch_one(&self.pool)
                .await?;
        Ok(count != 0)
    }

    /// Remove the registry row in the caller's revocation transaction. The
    /// returned token path is deleted after commit.
    pub(crate) async fn take_for_delete(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        device_id: Uuid,
        principal: Uuid,
    ) -> IdentityResult<Option<PathBuf>> {
        let path: Option<String> = sqlx::query_scalar(
            "SELECT token_path FROM device_connections WHERE device_id=? AND principal=?",
        )
        .bind(device_id.to_string())
        .bind(principal.to_string())
        .fetch_optional(&mut **tx)
        .await?;
        sqlx::query("DELETE FROM device_connections WHERE device_id=? AND principal=?")
            .bind(device_id.to_string())
            .bind(principal.to_string())
            .execute(&mut **tx)
            .await?;
        Ok(path.map(PathBuf::from))
    }

    pub(crate) fn remove_token(path: Option<PathBuf>) {
        let Some(path) = path else {
            return;
        };
        if let Err(error) = std::fs::remove_file(&path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    component = "device_connection",
                    "could not remove connection token file"
                );
            }
        }
    }
}

async fn write_token(dir: PathBuf, path: PathBuf, token: String) -> IdentityResult<()> {
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Do not trust an existing file's mode if it predates this writer.
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(token.as_bytes())?;
        file.sync_all()
    })
    .await
    .map_err(|_| IdentityError::Crypto)??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::CloudJournal;
    use peri_acp_types::device_enrollment::DeviceRecord;

    async fn fixture() -> (ConnectionStore, SqlitePool, tempfile::TempDir, Uuid, Uuid) {
        let root = tempfile::tempdir().unwrap();
        let journal = CloudJournal::open(root.path()).await.unwrap();
        let pool = journal.identity_pool();
        let principal = Uuid::new_v4();
        let device = Uuid::new_v4();
        sqlx::query("INSERT INTO principals(id,login,display_name,password_hash) VALUES(?,?,?,?)")
            .bind(principal.to_string())
            .bind("connection-owner")
            .bind("Owner")
            .bind("hash")
            .execute(&pool)
            .await
            .unwrap();
        let record = DeviceRecord {
            id: device,
            name: "PC".into(),
            platform: std::env::consts::OS.into(),
            default_workspace: root.path().to_str().unwrap().into(),
            connection_id: None,
            is_local: false,
            revoked: false,
        };
        sqlx::query("INSERT INTO identity_devices(id,principal,record_json) VALUES(?,?,?)")
            .bind(device.to_string())
            .bind(principal.to_string())
            .bind(serde_json::to_string(&record).unwrap())
            .execute(&pool)
            .await
            .unwrap();
        let store = ConnectionStore::new(pool.clone(), root.path());
        (store, pool, root, principal, device)
    }

    fn report(device: Uuid) -> ConnectionReport {
        ConnectionReport {
            device_id: device,
            endpoint: "127.0.0.1:45678".into(),
            transport_token: "a".repeat(64),
            lease_seconds: 90,
        }
    }

    #[tokio::test]
    async fn report_is_leased_persisted_and_revocation_removes_the_token() {
        let (store, pool, root, principal, device) = fixture().await;
        let receipt = store
            .publish(principal, report(device), 1_000)
            .await
            .unwrap();
        assert_eq!(receipt.lease_expires_at, 1_090);
        let active = store.active(device, 1_089).await.unwrap().unwrap();
        assert_eq!(active.endpoint, "127.0.0.1:45678".parse().unwrap());
        assert_eq!(
            std::fs::read_to_string(&active.token_path).unwrap(),
            "a".repeat(64)
        );
        assert!(store.active(device, 1_091).await.unwrap().is_none());
        assert!(store.exists(device).await.unwrap());
        let row: String = sqlx::query_scalar("SELECT record_json FROM identity_devices WHERE id=?")
            .bind(device.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        let record: DeviceRecord = serde_json::from_str(&row).unwrap();
        assert_eq!(record.connection_id, Some(receipt.connection_id));

        let mut tx = pool.begin().await.unwrap();
        let token = store
            .take_for_delete(&mut tx, device, principal)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert!(!store.exists(device).await.unwrap());
        ConnectionStore::remove_token(token);
        assert!(!active.token_path.exists());
        assert!(root.path().join("connections").exists());
    }

    #[tokio::test]
    async fn report_rejects_non_loopback_short_tokens_and_revoked_devices() {
        let (store, pool, _root, principal, device) = fixture().await;
        let mut invalid = report(device);
        invalid.endpoint = "10.0.0.1:45678".into();
        assert!(matches!(
            store.publish(principal, invalid, 1_000).await,
            Err(IdentityError::Invalid)
        ));
        let mut invalid = report(device);
        invalid.transport_token = "short".into();
        assert!(matches!(
            store.publish(principal, invalid, 1_000).await,
            Err(IdentityError::Invalid)
        ));
        let mut invalid = report(device);
        invalid.lease_seconds = 5;
        assert!(matches!(
            store.publish(principal, invalid, 1_000).await,
            Err(IdentityError::Invalid)
        ));
        let row: String = sqlx::query_scalar("SELECT record_json FROM identity_devices WHERE id=?")
            .bind(device.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        let mut record: DeviceRecord = serde_json::from_str(&row).unwrap();
        record.revoked = true;
        sqlx::query("UPDATE identity_devices SET record_json=? WHERE id=?")
            .bind(serde_json::to_string(&record).unwrap())
            .bind(device.to_string())
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            store.publish(principal, report(device), 1_000).await,
            Err(IdentityError::Forbidden)
        ));
    }

    #[tokio::test]
    async fn local_instance_cannot_report_a_connection() {
        let (store, pool, _root, principal, device) = fixture().await;
        let row: String = sqlx::query_scalar("SELECT record_json FROM identity_devices WHERE id=?")
            .bind(device.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        let mut record: DeviceRecord = serde_json::from_str(&row).unwrap();
        record.is_local = true;
        sqlx::query("UPDATE identity_devices SET record_json=? WHERE id=?")
            .bind(serde_json::to_string(&record).unwrap())
            .bind(device.to_string())
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            store.publish(principal, report(device), 1_000).await,
            Err(IdentityError::Forbidden)
        ));
    }
}
