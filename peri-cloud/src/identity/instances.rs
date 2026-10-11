use super::{DeviceRecord, IdentityError, IdentityResult, IdentityService};
use sqlx::Row;
use uuid::Uuid;

impl IdentityService {
    /// Deployment configuration grants the sole initialized personal account a
    /// same-host instance. Native OAuth remains required for external executors.
    pub(crate) async fn configure_local_instance(
        &self,
        record: DeviceRecord,
    ) -> IdentityResult<()> {
        *self.local_instance.write() = Some(record);
        self.ensure_local_instance().await
    }

    pub(crate) async fn ensure_local_instance(&self) -> IdentityResult<()> {
        let Some(record) = self.local_instance.read().clone() else {
            return Ok(());
        };
        sqlx::query("INSERT OR IGNORE INTO identity_devices(id,principal,record_json) SELECT ?,id,? FROM principals ORDER BY rowid LIMIT 1")
            .bind(record.id.to_string()).bind(serde_json::to_string(&record)?).execute(&self.pool).await?;
        // INSERT OR IGNORE deliberately preserves a revoked local instance.
        Ok(())
    }

    pub(crate) async fn apply_instance_defaults(
        &self,
        principal: Uuid,
        devices: &mut [DeviceRecord],
    ) -> IdentityResult<()> {
        let rows =
            sqlx::query("SELECT device_id,workspace FROM instance_defaults WHERE principal=?")
                .bind(principal.to_string())
                .fetch_all(&self.pool)
                .await?;
        for row in rows {
            if let Some(device) = devices
                .iter_mut()
                .find(|d| d.id.to_string() == row.get::<String, _>("device_id"))
            {
                device.default_workspace = row.get("workspace");
            }
        }
        Ok(())
    }

    /// Caller must first validate this directory through its owned executor.
    pub(crate) async fn set_instance_default(
        &self,
        principal: Uuid,
        id: Uuid,
        workspace: &str,
    ) -> IdentityResult<()> {
        let mut tx = self.pool.begin().await?;
        let row =
            sqlx::query("SELECT record_json FROM identity_devices WHERE principal=? AND id=?")
                .bind(principal.to_string())
                .bind(id.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(IdentityError::Forbidden)?;
        let device: DeviceRecord = serde_json::from_str(row.get("record_json"))?;
        if device.revoked {
            return Err(IdentityError::Forbidden);
        }
        sqlx::query("INSERT INTO instance_defaults(principal,device_id,workspace) VALUES(?,?,?) ON CONFLICT(principal,device_id) DO UPDATE SET workspace=excluded.workspace")
            .bind(principal.to_string()).bind(id.to_string()).bind(workspace).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
}
