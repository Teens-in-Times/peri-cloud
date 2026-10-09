use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{LoginError, Result};

/// Contains credentials; intentionally has no Debug implementation.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Credential {
    pub origin: String,
    pub principal_id: Uuid,
    pub device_id: Uuid,
    pub access_token: String,
    pub refresh_token: String,
    pub access_expires_at: i64,
    pub refresh_pending: bool,
}

pub(super) struct Vault {
    directory: PathBuf,
    key: String,
    _lock: File,
}

impl Vault {
    pub fn open(root: &Path, origin: &str, device_id: Uuid) -> Result<Self> {
        let directory = root.join("native-login");
        #[cfg(unix)]
        super::unix::private_directory(&directory)?;
        #[cfg(not(unix))]
        std::fs::create_dir_all(&directory).map_err(|_| LoginError::Storage)?;
        let mut digest = Sha256::new();
        digest.update(root.as_os_str().as_encoded_bytes());
        digest.update([0]);
        digest.update(origin.as_bytes());
        digest.update([0]);
        digest.update(device_id.as_bytes());
        let key = format!("{:x}", digest.finalize());
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options
            .open(directory.join(format!("{key}.lock")))
            .map_err(|_| LoginError::Storage)?;
        lock.try_lock_exclusive().map_err(|error| {
            if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
                LoginError::Busy
            } else {
                LoginError::Storage
            }
        })?;
        Ok(Self {
            directory,
            key,
            _lock: lock,
        })
    }

    pub fn load(&self) -> Result<Option<Credential>> {
        #[cfg(windows)]
        let bytes = super::windows::read(
            &self.directory.join(format!("{}.dpapi", self.key)),
            self.key.as_bytes(),
        )?;
        #[cfg(unix)]
        let bytes = super::unix::read(&self.directory.join(format!("{}.json", self.key)))?;
        bytes
            .map(|bytes| serde_json::from_slice(&bytes).map_err(|_| LoginError::InvalidCredential))
            .transpose()
    }

    pub fn save(&self, credential: &Credential) -> Result<()> {
        let bytes = serde_json::to_vec(credential).map_err(|_| LoginError::InvalidCredential)?;
        #[cfg(windows)]
        {
            super::windows::write(
                &self.directory.join(format!("{}.dpapi", self.key)),
                &bytes,
                self.key.as_bytes(),
            )
        }
        #[cfg(unix)]
        {
            super::unix::write(&self.directory.join(format!("{}.json", self.key)), &bytes)
        }
    }

    pub fn delete(&self) -> Result<()> {
        #[cfg(windows)]
        return match std::fs::remove_file(self.directory.join(format!("{}.dpapi", self.key))) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(LoginError::Storage),
        };
        #[cfg(unix)]
        match std::fs::remove_file(self.directory.join(format!("{}.json", self.key))) {
            Ok(()) => std::fs::File::open(&self.directory)
                .and_then(|file| file.sync_all())
                .map_err(|_| LoginError::Storage),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(LoginError::Storage),
        }
    }
}

#[cfg(test)]
#[path = "vault_test.rs"]
mod tests;
