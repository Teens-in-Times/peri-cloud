use std::fs::OpenOptions;
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};
use windows_sys::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};

use super::{LoginError, Result};

struct ProtectedBlob(CRYPT_INTEGER_BLOB);
impl Drop for ProtectedBlob {
    fn drop(&mut self) {
        // SAFETY: DPAPI owns this LocalAlloc allocation, including on early return.
        if !self.0.pbData.is_null() {
            unsafe {
                LocalFree(self.0.pbData.cast());
            }
        }
    }
}

pub(super) fn read(path: &Path, entropy: &[u8]) -> Result<Option<Vec<u8>>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(LoginError::Storage),
    };
    if !metadata.is_file() || metadata.len() > 16 * 1024 {
        return Err(LoginError::Storage);
    }
    let bytes = std::fs::read(path).map_err(|_| LoginError::Storage)?;
    transform(&bytes, entropy, false).map(Some)
}

pub(super) fn write(path: &Path, bytes: &[u8], entropy: &[u8]) -> Result<()> {
    let encrypted = transform(bytes, entropy, true)?;
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|_| LoginError::Storage)?;
        file.write_all(&encrypted)
            .map_err(|_| LoginError::Storage)?;
        file.sync_all().map_err(|_| LoginError::Storage)?;
        drop(file);
        let source = wide_path(&temporary);
        let target = wide_path(path);
        // SAFETY: both paths are NUL terminated; the temporary file is owned
        // by this writer and resides alongside the destination.
        if unsafe {
            MoveFileExW(
                source.as_ptr(),
                target.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(LoginError::Storage);
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

fn transform(bytes: &[u8], entropy: &[u8], protect: bool) -> Result<Vec<u8>> {
    if bytes.is_empty() || bytes.len() > 16 * 1024 {
        return Err(LoginError::InvalidCredential);
    }
    let mut input = bytes.to_vec();
    let mut entropy = entropy.to_vec();
    let input = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_mut_ptr(),
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: entropy.len() as u32,
        pbData: entropy.as_mut_ptr(),
    };
    let mut output = ProtectedBlob(CRYPT_INTEGER_BLOB::default());
    // SAFETY: buffers remain alive through the call and output is freed by
    // ProtectedBlob. No machine-wide flag or interactive prompt is enabled.
    let success = unsafe {
        if protect {
            CryptProtectData(
                &input,
                std::ptr::null(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output.0,
            )
        } else {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output.0,
            )
        }
    };
    if success == 0
        || output.0.pbData.is_null()
        || output.0.cbData == 0
        || output.0.cbData > 16 * 1024
    {
        return Err(LoginError::InvalidCredential);
    }
    // SAFETY: DPAPI returned this exact allocation extent.
    Ok(unsafe { std::slice::from_raw_parts(output.0.pbData, output.0.cbData as usize) }.to_vec())
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}
