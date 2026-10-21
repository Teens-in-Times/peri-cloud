use std::fmt::Write as _;
use std::fs;
use std::path::{Component, Path};

use peri_process::filesystem::directory_identity;
use sha2::{Digest, Sha256};

use crate::{Error, Result};
use peri_acp_types::device_executor::{
    SessionBinding, WorkspaceFile, WorkspaceIdentity, WorkspaceInstructions, WorkspaceSkills,
};

/// The cloud injects project instructions into the system prompt, so this
/// endpoint refuses oversized snapshots instead of silently returning a prefix.
const MAX_INSTRUCTION_BYTES: u64 = 512 * 1024;
const MAX_SKILL_BYTES: u64 = 256 * 1024;
const MAX_SKILLS: usize = 512;
const MAX_SKILL_DEPTH: usize = 6;
const MAX_SKILL_DIRS: usize = 1000;
const MAX_RELATIVE_BYTES: usize = 4096;
const INSTRUCTION_CANDIDATES: [&str; 3] = ["AGENTS.md", "CLAUDE.md", ".claude/AGENTS.md"];

pub(crate) async fn identity(path: &str) -> Result<WorkspaceIdentity> {
    let path = path.to_owned();
    let evidence =
        tokio::task::spawn_blocking(move || directory_identity(Path::new(&path))).await??;
    Ok(WorkspaceIdentity {
        device: evidence.device,
        inode: evidence.inode,
    })
}

pub(crate) async fn verify(binding: &SessionBinding) -> Result<()> {
    let actual = identity(&binding.workspace)
        .await
        .map_err(|_| Error::WorkspaceChanged)?;
    if actual != binding.workspace_identity {
        return Err(Error::WorkspaceChanged);
    }
    Ok(())
}

/// Resolve the same project instruction candidates the local workspace
/// provider uses. `@import` expansion is intentionally not performed here.
pub(crate) fn instructions(workspace: &str) -> Result<WorkspaceInstructions> {
    let root = Path::new(workspace);
    let main = INSTRUCTION_CANDIDATES
        .iter()
        .find_map(|candidate| instruction(root, candidate));
    let local = instruction(root, "CLAUDE.local.md");
    // SOUL belongs to the assistant, never to cwd. Main files are fallbacks,
    // not additive rules; do not load CLAUDE.md when AGENTS.md is present.
    let documents = main.iter().chain(local.iter()).cloned().collect();
    Ok(WorkspaceInstructions {
        main,
        local,
        documents,
    })
}

/// List `SKILL.md` files under `.claude/skills`. Symlinked directories and
/// files are not followed, matching the workspace provider's no-symlink rule.
pub(crate) fn skills(workspace: &str) -> WorkspaceSkills {
    let root = Path::new(workspace);
    let mut paths = Vec::new();
    let mut truncated = false;
    let mut visited = 0_usize;
    scan(
        root,
        ".claude/skills",
        0,
        &mut paths,
        &mut truncated,
        &mut visited,
    );
    scan(
        root,
        ".agents/skills",
        0,
        &mut paths,
        &mut truncated,
        &mut visited,
    );
    paths.sort();
    WorkspaceSkills { paths, truncated }
}

/// Read one skill entry. Callers can only reach paths that the discovery
/// shape allows, even if they construct the request themselves.
pub(crate) fn read_skill(workspace: &str, relative: &str) -> Result<WorkspaceFile> {
    if !valid_skill_path(relative) {
        return Err(Error::Invalid("not a skill entry path".into()));
    }
    let content =
        read_text(Path::new(workspace), relative, MAX_SKILL_BYTES)?.ok_or(Error::NotFound)?;
    Ok(WorkspaceFile {
        path: relative.to_owned(),
        digest: digest(content.as_bytes()),
        content,
    })
}

fn instruction(root: &Path, relative: &str) -> Option<WorkspaceFile> {
    match read_text(root, relative, MAX_INSTRUCTION_BYTES) {
        Ok(Some(content)) if !content.trim().is_empty() => Some(WorkspaceFile {
            path: relative.to_owned(),
            digest: digest(content.as_bytes()),
            content,
        }),
        Ok(_) => None,
        Err(error) => {
            tracing::debug!(path = relative, %error, "workspace instruction skipped");
            None
        }
    }
}

fn scan(
    root: &Path,
    relative: &str,
    depth: usize,
    paths: &mut Vec<String>,
    truncated: &mut bool,
    visited: &mut usize,
) {
    if *truncated || depth > MAX_SKILL_DEPTH {
        return;
    }
    if !no_symlink_ancestors(root, relative).unwrap_or(false) {
        return;
    }
    *visited += 1;
    if *visited > MAX_SKILL_DIRS {
        *truncated = true;
        return;
    }
    let directory = root.join(relative);
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            tracing::debug!(path = relative, %error, "workspace skill directory skipped");
            return;
        }
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .collect();
    names.sort();
    for name in names {
        let child = format!("{relative}/{name}");
        let metadata = match fs::symlink_metadata(root.join(&child)) {
            Ok(metadata) => metadata,
            Err(error) => {
                tracing::debug!(path = %child, %error, "workspace skill entry skipped");
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            scan(root, &child, depth + 1, paths, truncated, visited);
        } else if metadata.is_file() && name == "SKILL.md" {
            if paths.len() >= MAX_SKILLS {
                *truncated = true;
                return;
            }
            paths.push(child);
        }
    }
}

fn read_text(root: &Path, relative: &str, max_bytes: u64) -> Result<Option<String>> {
    if !valid_relative(relative) || !no_symlink_ancestors(root, relative)? {
        return Ok(None);
    }
    let path = root.join(relative);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(None);
    }
    if metadata.len() > max_bytes {
        return Err(Error::Invalid(format!(
            "workspace file exceeds {max_bytes} bytes"
        )));
    }
    let bytes = fs::read(&path)?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| Error::Invalid("workspace file is not UTF-8 text".into()))
}

/// Reject absolute paths, parent traversal, Windows separators/drive prefixes
/// and empty segments before joining anything to the bound workspace.
fn valid_relative(relative: &str) -> bool {
    if relative.is_empty()
        || relative.len() > MAX_RELATIVE_BYTES
        || relative.contains('\\')
        || relative.contains('\0')
        || relative.contains(':')
    {
        return false;
    }
    let mut normal = 0usize;
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(part) if !part.is_empty() => normal += 1,
            _ => return false,
        }
    }
    normal > 0
}

fn valid_skill_path(relative: &str) -> bool {
    if !valid_relative(relative)
        || !(relative.starts_with(".claude/skills/") || relative.starts_with(".agents/skills/"))
        || !relative.ends_with("/SKILL.md")
    {
        return false;
    }
    relative
        .strip_prefix(".claude/skills/")
        .or_else(|| relative.strip_prefix(".agents/skills/"))
        .and_then(|rest| rest.strip_suffix("/SKILL.md"))
        .is_some_and(|dir| !dir.is_empty())
}

/// Check every component with `symlink_metadata` rather than canonicalizing:
/// a symlinked parent could resolve inside the workspace and still change the
/// file between discovery and read.
fn no_symlink_ancestors(root: &Path, relative: &str) -> Result<bool> {
    let mut current = root.to_path_buf();
    for component in Path::new(relative).components() {
        let Component::Normal(part) = component else {
            return Ok(false);
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Ok(false),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(true)
}

fn digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(7 + digest.len() * 2);
    hex.push_str("sha256:");
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
#[path = "workspace_test.rs"]
mod tests;
