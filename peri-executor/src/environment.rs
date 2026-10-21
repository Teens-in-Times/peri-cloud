//! Bounded PATH discovery. No shell, profile, registry scan or binary is executed.
use peri_acp_types::device_executor::{
    EnvironmentApplication, EnvironmentSnapshot, ExecutorInfo, SessionBinding,
};
use std::path::{Path, PathBuf};

pub(crate) fn snapshot(info: &ExecutorInfo, binding: SessionBinding) -> EnvironmentSnapshot {
    let paths = std::env::var_os("PATH")
        .map(|v| std::env::split_paths(&v).take(128).collect::<Vec<_>>())
        .unwrap_or_default();
    EnvironmentSnapshot {
        device_id: info.device_id,
        binding,
        platform: std::env::consts::OS.into(),
        architecture: std::env::consts::ARCH.into(),
        shell: if cfg!(windows) {
            "PowerShell (-NoProfile)"
        } else {
            "bash (-c)"
        }
        .into(),
        observed_at: chrono::Utc::now().to_rfc3339(),
        applications: [
            "git", "cargo", "rustc", "node", "python", "python3", "bun", "go", "docker",
        ]
        .into_iter()
        .map(|name| EnvironmentApplication {
            name: name.into(),
            executable: locate(name, &paths).and_then(|p| p.to_str().map(str::to_owned)),
            version: None,
        })
        .collect(),
    }
}
fn locate(name: &str, paths: &[PathBuf]) -> Option<PathBuf> {
    for directory in paths {
        // Ignore relative/empty PATH entries; never attribute cwd executables to the system.
        if !directory.is_absolute() {
            continue;
        }
        let suffixes: &[&str] = if cfg!(windows) {
            &[".exe", ".cmd", ".bat"]
        } else {
            &[""]
        };
        for suffix in suffixes {
            let path = directory.join(format!("{name}{suffix}"));
            if executable(&path) {
                return Some(path);
            }
        }
    }
    None
}
fn executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_does_not_execute_candidates_or_search_relative_paths() {
        let root = tempfile::tempdir().unwrap();
        let suffix = if cfg!(windows) { ".cmd" } else { "" };
        let path = root.path().join(format!("git{suffix}"));
        std::fs::write(&path, "this is a probe fixture, never execute").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert_eq!(
            locate("git", &[PathBuf::from("."), root.path().into()]),
            Some(path)
        );
        assert!(locate("git", &[PathBuf::from(".")]).is_none());
    }
}
