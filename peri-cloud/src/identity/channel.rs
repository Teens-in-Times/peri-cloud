use sqlx::Row;
use uuid::Uuid;

use super::{ChannelIdentity, DeviceRecord, IdentityError, IdentityResult, IdentityService};

impl IdentityService {
    /// A gateway supplies only the sender observed by its authenticated adapter.
    /// Resolve account membership on every use, including interaction responses.
    pub async fn channel_devices(
        &self,
        identity: &ChannelIdentity,
    ) -> IdentityResult<(Uuid, Vec<DeviceRecord>)> {
        let principal = self.resolve_channel(identity).await?;
        let rows =
            sqlx::query("SELECT record_json FROM identity_devices WHERE principal=? ORDER BY id")
                .bind(principal.to_string())
                .fetch_all(&self.pool)
                .await?;
        let mut devices = Vec::new();
        for row in rows {
            let device: DeviceRecord = serde_json::from_str(row.get("record_json"))?;
            if !device.revoked {
                devices.push(device);
            }
        }
        Ok((principal, devices))
    }

    pub async fn channel_device(
        &self,
        identity: &ChannelIdentity,
        device: Uuid,
    ) -> IdentityResult<(Uuid, DeviceRecord)> {
        let (principal, devices) = self.channel_devices(identity).await?;
        let device = devices
            .into_iter()
            .find(|record| record.id == device)
            .ok_or(IdentityError::Forbidden)?;
        Ok((principal, device))
    }
}
