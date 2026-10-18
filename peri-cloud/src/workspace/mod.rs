//! Device workspace context for the cloud agent.
//!
//! Project instructions and the skill catalog are captured once per bound
//! session from the executor's workspace-read endpoints. Nothing falls back to
//! the cloud host's filesystem, and an executor that does not advertise the
//! capability simply contributes no workspace context.

mod skills;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use peri_acp_types::device_executor::WorkspaceFile;
use peri_acp_types::skills::{SkillMetadata, SkillSource};
use peri_agent::agent::model_bridge::SYSTEM_PROMPT_DYNAMIC_BOUNDARY;
use peri_middlewares::SkillsMiddleware;
use peri_remote_tools::RemoteSession;
use tokio::sync::Mutex;

pub(crate) use skills::CloudSkillsMiddleware;

/// Keep injected instructions bounded independently of the executor limit.
const MAX_MAIN_BYTES: usize = 64 * 1024;
const MAX_LOCAL_BYTES: usize = 16 * 1024;
const LOAD_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone, Debug)]
pub(crate) struct WorkspaceSkill {
    pub name: String,
    /// Workspace-relative path, always `.claude/skills/**/SKILL.md`.
    pub path: String,
}

#[async_trait]
pub(crate) trait SkillReader: Send + Sync {
    async fn read(&self, path: &str) -> Result<WorkspaceFile, String>;
}

struct RemoteSkillReader {
    remote: Arc<RemoteSession>,
}

#[async_trait]
impl SkillReader for RemoteSkillReader {
    async fn read(&self, path: &str) -> Result<WorkspaceFile, String> {
        match tokio::time::timeout(LOAD_TIMEOUT, self.remote.workspace_read(path)).await {
            Ok(Ok(file)) => Ok(file),
            Ok(Err(_)) => Err("the device workspace read failed".to_owned()),
            Err(_) => Err("the device workspace read timed out".to_owned()),
        }
    }
}

pub(crate) struct DeviceWorkspace {
    pub main: Option<String>,
    pub local: Option<String>,
    pub skills: Vec<WorkspaceSkill>,
    /// `false` when discovery failed or the executor lacks the capability.
    /// An empty catalog with `true` is a legitimate empty skill root.
    pub skills_available: bool,
    pub reader: Arc<dyn SkillReader>,
}

impl DeviceWorkspace {
    pub(crate) fn summary(&self) -> Option<String> {
        let metadata: Vec<SkillMetadata> = self
            .skills
            .iter()
            .map(|skill| SkillMetadata {
                name: skill.name.clone(),
                description: String::new(),
                source: SkillSource::Project,
                ..Default::default()
            })
            .collect();
        SkillsMiddleware::render_frozen_summary(&metadata)
    }

    pub(crate) fn has_instructions(&self) -> bool {
        self.main.is_some() || self.local.is_some()
    }
}

/// Per-session cache. The agent is cloned for scheduled turns, so the cache is
/// shared through an `Arc`; a different device or workspace uses a different
/// session binding and therefore a different cache.
pub(crate) struct WorkspaceCache {
    state: Mutex<Option<Arc<DeviceWorkspace>>>,
}

impl WorkspaceCache {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(None),
        }
    }

    pub(crate) async fn load(&self, remote: &Arc<RemoteSession>) -> Arc<DeviceWorkspace> {
        let mut state = self.state.lock().await;
        if let Some(workspace) = state.as_ref() {
            return workspace.clone();
        }
        let workspace = Arc::new(load(remote).await);
        *state = Some(workspace.clone());
        workspace
    }
}

async fn load(remote: &Arc<RemoteSession>) -> DeviceWorkspace {
    let reader: Arc<dyn SkillReader> = Arc::new(RemoteSkillReader {
        remote: remote.clone(),
    });
    if !remote.supports_workspace_read() {
        return DeviceWorkspace {
            main: None,
            local: None,
            skills: Vec::new(),
            skills_available: false,
            reader,
        };
    }
    let (instructions, listed) = tokio::join!(
        tokio::time::timeout(LOAD_TIMEOUT, remote.workspace_instructions()),
        tokio::time::timeout(LOAD_TIMEOUT, remote.workspace_skills()),
    );
    let instructions = instructions.ok().and_then(Result::ok);
    let main = instructions
        .as_ref()
        .and_then(|value| value.main.as_ref())
        .map(|file| cap_text(file.content.clone(), MAX_MAIN_BYTES));
    let local = instructions
        .as_ref()
        .and_then(|value| value.local.as_ref())
        .map(|file| cap_text(file.content.clone(), MAX_LOCAL_BYTES));
    let listed = listed.ok().and_then(Result::ok);
    let skills = listed
        .as_ref()
        .map(|value| catalog(value.paths.iter().map(String::as_str)))
        .unwrap_or_default();
    DeviceWorkspace {
        main,
        local,
        skills,
        skills_available: listed.is_some(),
        reader,
    }
}

fn catalog<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<WorkspaceSkill> {
    let mut unique: BTreeMap<String, String> = BTreeMap::new();
    for path in paths {
        if !valid_skill_path(path) {
            continue;
        }
        let Some(base) = display_name(&base_name(path)) else {
            continue;
        };
        unique.insert(path.to_owned(), base);
    }
    let mut counts = BTreeMap::<String, usize>::new();
    for name in unique.values() {
        *counts.entry(name.clone()).or_default() += 1;
    }
    let mut skills: Vec<WorkspaceSkill> = unique
        .into_iter()
        .map(|(path, base)| WorkspaceSkill {
            name: if counts.get(&base) == Some(&1) {
                base
            } else {
                relative_dir(&path).chars().take(120).collect()
            },
            path,
        })
        .collect();
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    skills
}

fn valid_skill_path(path: &str) -> bool {
    if path.len() > 4096
        || path.contains('\\')
        || path.contains('\0')
        || path.contains(':')
        || path.chars().any(char::is_control)
    {
        return false;
    }
    let Some(dir) = path
        .strip_prefix(".claude/skills/")
        .and_then(|rest| rest.strip_suffix("/SKILL.md"))
    else {
        return false;
    };
    !dir.is_empty()
        && path
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

/// Directory names are injected into the system prompt, so control characters
/// and unbounded length are rejected before they reach the catalog.
fn display_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().any(char::is_control) {
        return None;
    }
    Some(trimmed.chars().take(80).collect())
}

fn relative_dir(path: &str) -> &str {
    path.strip_prefix(".claude/skills/")
        .and_then(|rest| rest.strip_suffix("/SKILL.md"))
        .unwrap_or(path)
}

fn base_name(path: &str) -> String {
    relative_dir(path)
        .rsplit('/')
        .next()
        .unwrap_or("skill")
        .to_owned()
}

fn cap_text(text: String, max: usize) -> String {
    let text = text.replace(
        SYSTEM_PROMPT_DYNAMIC_BOUNDARY,
        "[reserved prompt boundary token removed]",
    );
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n[Cloud workspace: instructions truncated from {} bytes to {} bytes.]",
        &text[..end],
        text.len(),
        end
    )
}

#[cfg(test)]
mod tests;
