use super::*;
use peri_acp_types::device_enrollment::DeviceRecord;
use sqlx::{SqliteConnection, SqlitePool};

/// Selection is data; authorization always comes from the captured account.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MemoryScope {
    #[default]
    User,
    Fleet,
    DeviceEnv {
        device_id: Uuid,
    },
    Context {
        context_id: Uuid,
    },
}
impl MemoryScope {
    pub(crate) fn key(&self) -> String {
        match self {
            Self::User => "user".into(),
            Self::Fleet => "fleet".into(),
            Self::DeviceEnv { device_id } => format!("device_env:{device_id}"),
            Self::Context { context_id } => format!("context:{context_id}"),
        }
    }
    pub(crate) fn from_key(key: &str) -> MemoryResult<Self> {
        match key {
            "user" => Ok(Self::User),
            "fleet" => Ok(Self::Fleet),
            _ if key.starts_with("context:") => key
                .strip_prefix("context:")
                .and_then(|id| Uuid::parse_str(id).ok())
                .map(|context_id| Self::Context { context_id })
                .ok_or(MemoryError::Invalid),
            _ => key
                .strip_prefix("device_env:")
                .and_then(|id| Uuid::parse_str(id).ok())
                .map(|device_id| Self::DeviceEnv { device_id })
                .ok_or(MemoryError::Invalid),
        }
    }
    pub(crate) fn chars(&self) -> usize {
        match self {
            Self::User => 1200,
            Self::Fleet => 1500,
            _ => 2000,
        }
    }
    pub(crate) async fn authorize(
        &self,
        conn: &mut SqliteConnection,
        owner: Uuid,
    ) -> MemoryResult<()> {
        self.authorize_management(conn, owner, false).await
    }
    pub(crate) async fn authorize_management(
        &self,
        conn: &mut SqliteConnection,
        owner: Uuid,
        retained: bool,
    ) -> MemoryResult<()> {
        match self {
            Self::User | Self::Fleet => Ok(()),
            Self::Context { .. } => Err(MemoryError::Forbidden),
            Self::DeviceEnv { device_id } => {
                let json: Option<String> = sqlx::query_scalar(
                    "SELECT record_json FROM identity_devices WHERE principal=? AND id=?",
                )
                .bind(owner.to_string())
                .bind(device_id.to_string())
                .fetch_optional(conn)
                .await?;
                let record: DeviceRecord =
                    serde_json::from_str(&json.ok_or(MemoryError::Forbidden)?)?;
                if (record.revoked && !retained) || record.id != *device_id {
                    return Err(MemoryError::Forbidden);
                }
                Ok(())
            }
        }
    }
}

/// Only public identity metadata is selected. Registration says nothing about liveness.
pub(crate) async fn devices(pool: &SqlitePool, owner: Uuid) -> MemoryResult<Vec<DeviceRecord>> {
    Ok(owned_devices(pool, owner)
        .await?
        .into_iter()
        .filter(|d| !d.revoked)
        .collect())
}
pub(crate) async fn owned_devices(
    pool: &SqlitePool,
    owner: Uuid,
) -> MemoryResult<Vec<DeviceRecord>> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT record_json FROM identity_devices WHERE principal=? ORDER BY id",
    )
    .bind(owner.to_string())
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|s| serde_json::from_str::<DeviceRecord>(&s).map_err(MemoryError::from))
        .collect()
}
