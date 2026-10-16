//! Write/Edit 共用的进程内 per-target 事务。

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use parking_lot::Mutex as TargetMutex;
use peri_acp_types::sentinel::projection_sentinel_counts_v1;

pub(crate) const SENTINEL_REJECTION: &str =
    "File change rejected because the result contains newly introduced protected projection text.";

type Registry = Mutex<HashMap<PathBuf, Weak<TargetMutex<()>>>>;

pub(crate) struct LockedTarget<'a> {
    path: &'a Path,
}

pub(crate) fn with_target_lock<T>(path: &Path, operation: impl FnOnce(LockedTarget<'_>) -> T) -> T {
    let lock = target_lock(path);
    let _guard = lock.lock();
    operation(LockedTarget { path })
}

impl LockedTarget<'_> {
    pub(crate) fn read_pre(&self) -> std::io::Result<Option<Vec<u8>>> {
        match std::fs::read(self.path) {
            Ok(content) => Ok(Some(content)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn guard_and_commit(&self, pre: &[u8], post: &[u8]) -> Result<(), CommitError> {
        if introduces_projection_sentinel_bytes(pre, post) {
            return Err(CommitError::Sentinel);
        }

        let parent = self.path.parent().ok_or(CommitError::Io)?;
        if !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|_| CommitError::Io)?;
        }

        let tmp_path = self
            .path
            .with_extension(format!("tmp.{}", uuid::Uuid::now_v7()));
        if std::fs::write(&tmp_path, post).is_err() {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(CommitError::Io);
        }

        if let Ok(metadata) = std::fs::metadata(self.path) {
            #[cfg(unix)]
            if std::fs::set_permissions(&tmp_path, metadata.permissions()).is_err() {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(CommitError::Io);
            }
            #[cfg(not(unix))]
            let _ = metadata;
        }

        if rename_committed_file(&tmp_path, self.path).is_err() {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(CommitError::Io);
        }
        Ok(())
    }
}

#[cfg(not(windows))]
fn rename_committed_file(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(source, target)
}

#[cfg(windows)]
fn rename_committed_file(source: &Path, target: &Path) -> std::io::Result<()> {
    // A scanner or another path lookup can briefly hold a non-delete-shared
    // handle. Windows can report this as ACCESS_DENIED as well as a sharing or
    // lock violation. Retry these definite failures only, with the target lock
    // still held and the already-computed contents unchanged (including append).
    let mut delay_ms = 5;
    loop {
        match std::fs::rename(source, target) {
            Err(error)
                if delay_ms <= 80
                    && matches!(error.raw_os_error(), Some(5) | Some(32) | Some(33)) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                delay_ms *= 2;
            }
            result => return result,
        }
    }
}

fn introduces_projection_sentinel_bytes(pre: &[u8], post: &[u8]) -> bool {
    let counts = |bytes: &[u8]| {
        let mut counts = HashMap::new();
        for line in bytes.split(|byte| *byte == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let Ok(line) = std::str::from_utf8(line) else {
                continue;
            };
            for (sentinel, count) in projection_sentinel_counts_v1(line) {
                *counts.entry(sentinel).or_default() += count;
            }
        }
        counts
    };
    let pre_counts = counts(pre);
    counts(post)
        .into_iter()
        .any(|(sentinel, count)| count > pre_counts.get(&sentinel).copied().unwrap_or(0))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CommitError {
    Sentinel,
    Io,
}

pub(crate) fn target_key(cwd: &str, file_path: &str) -> PathBuf {
    let path = Path::new(file_path);
    let raw = if path.is_absolute() {
        path.to_path_buf()
    } else {
        Path::new(cwd).join(path)
    };
    let normalized = logical_path_key(&raw);
    if let Ok(canonical) = normalized.canonicalize() {
        return canonical;
    }

    let mut ancestor = normalized.as_path();
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        let Some(name) = ancestor.file_name() else {
            return normalized;
        };
        suffix.push(name.to_os_string());
        let Some(parent) = ancestor.parent() else {
            return normalized;
        };
        ancestor = parent;
    }
    let Ok(mut key) = ancestor.canonicalize() else {
        return normalized;
    };
    for component in suffix.into_iter().rev() {
        key.push(component);
    }
    key
}

pub(crate) fn logical_path_key(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn target_lock(path: &Path) -> Arc<TargetMutex<()>> {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    let registry = REGISTRY.get_or_init(Default::default);
    let key = logical_path_key(path);
    let mut entries = registry.lock().unwrap();
    entries.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = entries.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(TargetMutex::new(()));
    entries.insert(key, Arc::downgrade(&lock));
    lock
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;

    #[test]
    fn commit_tolerates_a_brief_windows_sharing_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.txt");
        std::fs::write(&path, b"before").unwrap();
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(3) // FILE_SHARE_READ | FILE_SHARE_WRITE, without DELETE.
            .open(&path)
            .unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            drop(handle);
        });
        let result = with_target_lock(&path, |locked| locked.guard_and_commit(b"before", b"after"));
        release.join().unwrap();
        result.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"after");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn persistent_sharing_lock_preserves_original_and_cleans_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.txt");
        std::fs::write(&path, b"before").unwrap();
        let _handle = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(&path)
            .unwrap();
        assert_eq!(
            with_target_lock(&path, |locked| locked.guard_and_commit(b"before", b"after")),
            Err(CommitError::Io)
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"before");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
