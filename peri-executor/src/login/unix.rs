use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use super::{LoginError, Result};

pub(super) fn private_directory(directory: &Path) -> Result<()> {
    match std::fs::create_dir(directory) {
        Ok(()) => std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| LoginError::Storage)?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(LoginError::Storage),
    }
    let metadata = std::fs::symlink_metadata(directory).map_err(|_| LoginError::Storage)?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(LoginError::Storage);
    }
    Ok(())
}

pub(super) fn read(path: &Path) -> Result<Option<Vec<u8>>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(LoginError::Storage),
    };
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.len() > 16 * 1024
    {
        return Err(LoginError::Storage);
    }
    std::fs::read(path)
        .map(Some)
        .map_err(|_| LoginError::Storage)
}

pub(super) fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|_| LoginError::Storage)?;
        file.write_all(bytes).map_err(|_| LoginError::Storage)?;
        file.sync_all().map_err(|_| LoginError::Storage)?;
        std::fs::rename(&temporary, path).map_err(|_| LoginError::Storage)?;
        std::fs::File::open(path.parent().ok_or(LoginError::Storage)?)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| LoginError::Storage)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}
