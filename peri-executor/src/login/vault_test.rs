use super::*;

fn credential(origin: &str, device_id: Uuid) -> Credential {
    Credential {
        origin: origin.into(),
        principal_id: Uuid::new_v4(),
        device_id,
        access_token: "a".repeat(64),
        refresh_token: "b".repeat(64),
        access_expires_at: 1900000000,
        refresh_pending: false,
    }
}

struct Cleanup {
    root: PathBuf,
    origin: String,
    device_id: Uuid,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Ok(vault) = Vault::open(&self.root, &self.origin, self.device_id) {
            let _ = vault.delete();
        }
    }
}

#[test]
fn test_vault_reopen_preserves_credentials_and_excludes_second_owner() {
    let directory = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(directory.path()).unwrap();
    let origin = "https://fixture.agent.test";
    let device_id = Uuid::new_v4();
    let _cleanup = Cleanup {
        root: root.clone(),
        origin: origin.into(),
        device_id,
    };
    let vault = Vault::open(&root, origin, device_id).unwrap();
    assert!(vault.load().unwrap().is_none());
    let mut saved = credential(origin, device_id);
    vault.save(&saved).unwrap();
    assert!(matches!(
        Vault::open(&root, origin, device_id),
        Err(LoginError::Busy)
    ));
    saved.refresh_pending = true;
    vault.save(&saved).unwrap();
    drop(vault);
    let reopened = Vault::open(&root, origin, device_id).unwrap();
    let loaded = reopened.load().unwrap().unwrap();
    assert_eq!(loaded.principal_id, saved.principal_id);
    assert_eq!(loaded.access_token, saved.access_token);
    assert_eq!(loaded.refresh_token, saved.refresh_token);
    assert!(loaded.refresh_pending, "不确定刷新状态必须跨重启保留");
    reopened.delete().unwrap();
    assert!(reopened.load().unwrap().is_none());
}

#[cfg(unix)]
#[test]
fn test_vault_rejects_public_file_and_directory_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let device_id = Uuid::new_v4();
    let vault = Vault::open(directory.path(), "https://fixture.test", device_id).unwrap();
    vault
        .save(&credential("https://fixture.test", device_id))
        .unwrap();
    let path = vault.directory.join(format!("{}.json", vault.key));
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(vault.load(), Err(LoginError::Storage)));
    drop(vault);
    std::fs::set_permissions(
        directory.path().join("native-login"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert!(matches!(
        Vault::open(directory.path(), "https://fixture.test", device_id),
        Err(LoginError::Storage)
    ));
}

#[cfg(windows)]
#[test]
fn test_vault_windows_ciphertext_rejects_tamper_and_different_binding() {
    let directory = tempfile::tempdir().unwrap();
    let device_id = Uuid::new_v4();
    let origin = "https://fixture.private.test";
    let vault = Vault::open(directory.path(), origin, device_id).unwrap();
    let saved = credential(origin, device_id);
    vault.save(&saved).unwrap();
    let path = vault.directory.join(format!("{}.dpapi", vault.key));
    let mut encrypted = std::fs::read(&path).unwrap();
    assert!(
        !encrypted
            .windows(64)
            .any(|window| window == saved.access_token.as_bytes()),
        "落盘内容不可包含明文凭据"
    );
    assert!(matches!(
        crate::login::windows::read(&path, b"different-origin-device-binding"),
        Err(LoginError::InvalidCredential)
    ));
    let last = encrypted.len() - 1;
    encrypted[last] ^= 1;
    std::fs::write(&path, encrypted).unwrap();
    assert!(matches!(vault.load(), Err(LoginError::InvalidCredential)));
}
