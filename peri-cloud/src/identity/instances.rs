use super::{DeviceRecord, IdentityError, IdentityResult, IdentityService};
use sqlx::Row;
use uuid::Uuid;

impl IdentityService {
    /// Server execution belongs only to the original administrator account.
    /// Invited accounts never inherit it; external executors still use OAuth.
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
        let mut tx = self.pool.begin().await?;
        let owner: Option<String> = sqlx::query_scalar("SELECT p.id FROM principals p JOIN account_roles r ON r.principal=p.id WHERE r.admin=1 ORDER BY p.rowid LIMIT 1")
            .fetch_optional(&mut *tx).await?;
        let Some(owner) = owner else {
            return Ok(()); // Initial setup will claim it after creating the administrator.
        };
        if let Some(row) =
            sqlx::query("SELECT principal,record_json FROM identity_devices WHERE id=?")
                .bind(record.id.to_string())
                .fetch_optional(&mut *tx)
                .await?
        {
            let existing: DeviceRecord = serde_json::from_str(row.get("record_json"))?;
            if row.get::<String, _>("principal") != owner || !existing.is_local {
                return Err(IdentityError::Forbidden);
            }
        }
        sqlx::query(
            "INSERT OR IGNORE INTO identity_devices(id,principal,record_json) VALUES(?,?,?)",
        )
        .bind(record.id.to_string())
        .bind(owner)
        .bind(serde_json::to_string(&record)?)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::CloudJournal;

    #[tokio::test]
    async fn configured_server_id_cannot_claim_an_existing_guest_device() {
        let root = tempfile::tempdir().unwrap();
        let journal = CloudJournal::open(root.path()).await.unwrap();
        let bootstrap = "fixture-bootstrap-32-character-secret";
        let identity = IdentityService::new(&journal, bootstrap).await.unwrap();
        let owner = identity
            .initialize(bootstrap, "owner", "fixture-password-12".into(), "Owner")
            .await
            .unwrap();
        let login = identity
            .login("owner", "fixture-password-12".into())
            .await
            .unwrap();
        let actor = identity
            .authenticate_browser(&login.session_token)
            .await
            .unwrap();
        let invite = identity.create_invite(&actor).await.unwrap();
        let guest = identity
            .register_invited(&invite.code, "guest", "fixture-password-12".into(), "Guest")
            .await
            .unwrap();
        let id = Uuid::new_v4();
        let record = DeviceRecord {
            id,
            name: "Guest device".into(),
            platform: "linux".into(),
            default_workspace: "/workspace".into(),
            connection_id: None,
            is_local: false,
            revoked: false,
        };
        sqlx::query("INSERT INTO identity_devices(id,principal,record_json) VALUES(?,?,?)")
            .bind(id.to_string())
            .bind(guest.id.to_string())
            .bind(serde_json::to_string(&record).unwrap())
            .execute(&identity.pool)
            .await
            .unwrap();
        let local = DeviceRecord {
            is_local: true,
            ..record.clone()
        };
        assert!(matches!(
            identity.configure_local_instance(local).await,
            Err(IdentityError::Forbidden)
        ));
        let assigned: String =
            sqlx::query_scalar("SELECT principal FROM identity_devices WHERE id=?")
                .bind(id.to_string())
                .fetch_one(&identity.pool)
                .await
                .unwrap();
        assert_eq!(assigned, guest.id.to_string());
        assert_ne!(assigned, owner.id.to_string());
        // A revoked administrator instance remains revoked after restart.
        let revoked = DeviceRecord {
            id: Uuid::new_v4(),
            is_local: true,
            revoked: true,
            ..record
        };
        sqlx::query("INSERT INTO identity_devices(id,principal,record_json) VALUES(?,?,?)")
            .bind(revoked.id.to_string())
            .bind(owner.id.to_string())
            .bind(serde_json::to_string(&revoked).unwrap())
            .execute(&identity.pool)
            .await
            .unwrap();
        identity
            .configure_local_instance(DeviceRecord {
                revoked: false,
                ..revoked.clone()
            })
            .await
            .unwrap();
        let saved: String =
            sqlx::query_scalar("SELECT record_json FROM identity_devices WHERE id=?")
                .bind(revoked.id.to_string())
                .fetch_one(&identity.pool)
                .await
                .unwrap();
        assert!(
            serde_json::from_str::<DeviceRecord>(&saved)
                .unwrap()
                .revoked
        );
        journal.close().await;
    }
}
