//! Native tool output persistence and UTF-8-safe truncation.

// ── 输出截断落盘（bg shell 执行链共用）───────────────────────────────────────

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::private_temp::{self, OUTPUT_PREFIX};

const LEGACY_OUTPUT_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// The upstream process-private, owner-only output store.
pub fn write_private_output(full_content: &str) -> std::io::Result<PathBuf> {
    private_temp::write_output(full_content)
}

pub fn persist_truncated_output(full_content: &str) -> String {
    persist_truncated_output_with_ref(full_content).0
}

/// Persist the full result and expose its reference separately from the display text.
pub fn persist_truncated_output_with_ref(full_content: &str) -> (String, Option<String>) {
    match write_private_output(full_content) {
        Ok(path) => (
            format!(
                "\n\n[Full output saved to {} — use `Read` to view complete content]",
                path.display()
            ),
            Some(path.to_string_lossy().into_owned()),
        ),
        Err(error) => (format!("\n\n[Failed to save full output: {error}]"), None),
    }
}

/// Sweep abandoned private directories and pre-upgrade flat output files.
pub fn cleanup_persisted_outputs() -> usize {
    let parent = std::env::temp_dir();
    private_temp::sweep_stale_dirs(&parent);
    cleanup_legacy_outputs(&parent, SystemTime::now())
}

fn cleanup_legacy_outputs(parent: &Path, now: SystemTime) -> usize {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|name| name.strip_prefix(OUTPUT_PREFIX))
            .and_then(|name| name.strip_suffix(".txt"))
        else {
            continue;
        };
        if uuid::Uuid::parse_str(id).is_err() {
            continue;
        }
        let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if metadata.is_file()
            && metadata
                .modified()
                .ok()
                .and_then(|time| now.duration_since(time).ok())
                .is_some_and(|age| age >= LEGACY_OUTPUT_MAX_AGE)
            && std::fs::remove_file(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

/// 按字节截断字符串，确保不拆分 UTF-8 字符边界。
///
/// 与 `&s[..max_bytes]` 不同，此函数会从 `max_bytes` 位置向前搜索
/// 最近的字符边界，避免在多字节字符中间截断。
pub fn truncate_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_removes_only_stale_legacy_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir
            .path()
            .join(format!("{OUTPUT_PREFIX}{}.txt", uuid::Uuid::new_v4()));
        let fresh = dir
            .path()
            .join(format!("{OUTPUT_PREFIX}{}.txt", uuid::Uuid::new_v4()));
        let foreign = dir.path().join(format!("{OUTPUT_PREFIX}unrelated.txt"));
        let now = SystemTime::now();
        for path in [&stale, &fresh, &foreign] {
            std::fs::write(path, b"x").unwrap();
        }
        for path in [&stale, &foreign] {
            let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            file.set_modified(now - LEGACY_OUTPUT_MAX_AGE * 2).unwrap();
        }
        assert_eq!(cleanup_legacy_outputs(dir.path(), now), 1);
        assert!(!stale.exists());
        assert!(fresh.exists());
        assert!(foreign.exists());
    }
}
