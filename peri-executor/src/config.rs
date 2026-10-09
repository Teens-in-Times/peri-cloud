use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::{Error, Result};
use peri_acp_types::device_executor::{ExecutorInfo, PROTOCOL_VERSION};

#[derive(Serialize, Deserialize)]
struct DeviceIdentity {
    id: Uuid,
    name: String,
}

pub(crate) struct DeviceConfig {
    pub info: ExecutorInfo,
    secret_hash: [u8; 32],
    state_owner: Arc<File>,
}

impl DeviceConfig {
    /// Login can read the immutable identity while the executor owns its lock.
    pub fn enrollment_info(root: &Path, name: &str) -> Result<ExecutorInfo> {
        let path = root.join("device.json");
        if path.exists() {
            let identity: DeviceIdentity = serde_json::from_slice(&std::fs::read(path)?)?;
            return Ok(ExecutorInfo {
                protocol_version: PROTOCOL_VERSION,
                device_id: identity.id,
                device_name: identity.name,
                platform: std::env::consts::OS.into(),
                version: env!("CARGO_PKG_VERSION").into(),
            });
        }
        Ok(Self::open(root, name)?.info)
    }

    pub fn open(root: &Path, name: &str) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("executor.lock"))?;
        lock.try_lock_exclusive().map_err(|error| {
            if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
                Error::AlreadyRunning
            } else {
                Error::Io(error)
            }
        })?;
        let path = root.join("device.json");
        let identity: DeviceIdentity = if path.exists() {
            serde_json::from_slice(&std::fs::read(path)?)?
        } else {
            let identity = DeviceIdentity {
                id: Uuid::new_v4(),
                name: name.to_owned(),
            };
            write_private(&path, &serde_json::to_vec_pretty(&identity)?)?;
            identity
        };
        let secret_path = root.join("transport-token");
        let secret = if secret_path.exists() {
            std::fs::read_to_string(secret_path)?.trim().to_owned()
        } else {
            let secret = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
            write_private(&secret_path, secret.as_bytes())?;
            secret
        };
        if secret.len() < 64 || secret.chars().any(char::is_whitespace) {
            return Err(Error::Invalid("invalid persisted transport token".into()));
        }
        Ok(Self {
            info: ExecutorInfo {
                protocol_version: PROTOCOL_VERSION,
                device_id: identity.id,
                device_name: identity.name,
                platform: std::env::consts::OS.into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            secret_hash: Sha256::digest(secret.as_bytes()).into(),
            state_owner: Arc::new(lock),
        })
    }

    pub fn verify(&self, secret: &str) -> bool {
        let candidate: [u8; 32] = Sha256::digest(secret.as_bytes()).into();
        bool::from(self.secret_hash.ct_eq(&candidate))
    }

    pub fn execution_lease(&self) -> Arc<File> {
        self.state_owner.clone()
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
