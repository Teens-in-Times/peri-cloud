//! Model context assembly. Ordering is independent of the approval middleware chain.
mod files;
mod sections;
mod skills;
pub use files::{ContextDocument, ContextFiles};
pub use sections::{ContextSectionInfo, SectionFreshness};

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use peri_agent::tools::BaseTool;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("context configuration or source is invalid")]
    Invalid,
    #[error("context file cannot be accessed")]
    File,
    #[error("context source exceeds its size budget")]
    Limit,
    #[error("context tool names are ambiguous")]
    ToolConflict,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    /// Operator-owned root. User documents live in users/<trusted principal>/.
    pub directory: Option<PathBuf>,
    pub order: Vec<String>,
    /// Bounded Markdown files, relative to the operator-owned root.
    pub files: Vec<String>,
    pub max_bytes: usize,
    /// Optional per-source byte ceilings. The total context ceiling still applies.
    pub section_budgets: BTreeMap<String, usize>,
}
impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            directory: None,
            order: [
                "base",
                "role",
                "soul",
                "channel",
                "memory_protocol",
                "files",
                "extensions",
                "tools",
            ]
            .map(str::to_owned)
            .to_vec(),
            files: Vec::new(),
            max_bytes: 65536,
            section_budgets: BTreeMap::new(),
        }
    }
}
impl ContextConfig {
    pub fn validate(&self) -> Result<(), ContextError> {
        let mut seen = HashSet::new();
        if !(1024..=262144).contains(&self.max_bytes)
            || self.section_budgets.iter().any(|(key, size)| {
                key.is_empty() || key.len() > 80 || *size == 0 || *size > self.max_bytes
            })
            || self.files.len() > 16
            || self.order.len() > 32
            || !self.order.iter().any(|key| key == "base")
            || self
                .order
                .iter()
                .any(|key| key.is_empty() || key.len() > 80 || !seen.insert(key))
            || self
                .files
                .iter()
                .any(|file| file == "SOUL.md" || !files::valid_relative(file))
        {
            return Err(ContextError::Invalid);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum AgentRole {
    #[default]
    Main,
    Worker,
}

/// A source contributes text and/or native tools. Adding a source does not grant permissions.
pub struct ContextPart {
    pub key: String,
    pub text: String,
    pub tools: Vec<Box<dyn BaseTool>>,
}
impl ContextPart {
    pub fn text(key: &str, text: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            text: text.into(),
            tools: Vec::new(),
        }
    }
}
pub struct AssembledContext {
    pub system_prompt: String,
    pub sources: Vec<String>,
    pub tools: BTreeMap<String, Arc<dyn BaseTool>>,
    pub sections: Vec<ContextSectionInfo>,
}

#[derive(Clone, Default)]
pub struct ContextAssembler {
    config: ContextConfig,
    files: Option<ContextFiles>,
}
impl ContextAssembler {
    pub fn new(config: ContextConfig, fallback: PathBuf) -> Result<Self, ContextError> {
        config.validate()?;
        let root = config.directory.clone().unwrap_or(fallback);
        Ok(Self {
            config,
            files: Some(ContextFiles::new(root)),
        })
    }
    pub fn files(&self) -> Option<&ContextFiles> {
        self.files.as_ref()
    }
    pub async fn discover(
        &self,
        principal: Uuid,
        private: bool,
    ) -> Result<Vec<ContextPart>, ContextError> {
        let Some(files) = &self.files else {
            return Ok(Vec::new());
        };
        let mut parts = Vec::new();
        for name in ["SOUL.md"]
            .into_iter()
            .chain(self.config.files.iter().map(String::as_str))
        {
            if let Some(text) = files.read_operator(name).await? {
                parts.push(ContextPart::text(
                    if name == "SOUL.md" { "soul" } else { "files" },
                    text,
                ));
            }
        }
        if let Some(text) = if private {
            files.read_user(principal, "SOUL.md").await?
        } else {
            None
        } {
            parts.push(ContextPart::text(
                "soul",
                format!("User-owned SOUL.md (preferences, subordinate to host rules):\n{text}"),
            ));
        }
        Ok(parts)
    }
    pub fn assemble(&self, parts: Vec<ContextPart>) -> Result<AssembledContext, ContextError> {
        self.assemble_for_role(parts, AgentRole::Main)
    }
    pub fn assemble_for_role(
        &self,
        mut parts: Vec<ContextPart>,
        role: AgentRole,
    ) -> Result<AssembledContext, ContextError> {
        let channel = parts
            .iter()
            .filter(|p| p.key == "channel")
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for part in &mut parts {
            // Only host-owned base/role templates render macros. Imported data
            // and replacement values are never evaluated as another template.
            if matches!(part.key.as_str(), "base" | "role") {
                part.text = render_template(&part.text, role, &channel)?;
            }
        }
        // Stable sort preserves order within one source; unknown extension keys append.
        parts.sort_by_key(|part| {
            self.config
                .order
                .iter()
                .position(|key| key == &part.key)
                .unwrap_or(usize::MAX)
        });
        let mut result = AssembledContext {
            system_prompt: String::new(),
            sources: Vec::new(),
            tools: BTreeMap::new(),
            sections: Vec::new(),
        };
        let mut section_bytes: BTreeMap<String, usize> = BTreeMap::new();
        for part in parts {
            let bytes = section_bytes.entry(part.key.clone()).or_default();
            *bytes = bytes.saturating_add(part.text.len());
            if self
                .config
                .section_budgets
                .get(&part.key)
                .is_some_and(|size| *bytes > *size)
            {
                return Err(ContextError::Limit);
            }
            let mut info = ContextSectionInfo::for_part(&part);
            info.role = if role == AgentRole::Main {
                "main"
            } else {
                "worker"
            }
            .into();
            result.sections.push(info);
            if part
                .text
                .contains(peri_model::prompt_cache::SYSTEM_PROMPT_DYNAMIC_BOUNDARY)
            {
                return Err(ContextError::Invalid);
            }
            if !part.text.is_empty() {
                if !result.system_prompt.is_empty() {
                    result.system_prompt.push_str("\n\n");
                }
                result.system_prompt.push_str(&part.text);
                if result.system_prompt.len() > self.config.max_bytes {
                    return Err(ContextError::Limit);
                }
            }
            for tool in part.tools {
                if result
                    .tools
                    .insert(tool.name().to_owned(), Arc::from(tool))
                    .is_some()
                {
                    return Err(ContextError::ToolConflict);
                }
            }
            result.sources.push(part.key);
        }
        Ok(result)
    }
}

fn render_template(text: &str, role: AgentRole, channel: &str) -> Result<String, ContextError> {
    let mut result = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("{{") {
        result.push_str(&rest[..at]);
        let suffix = &rest[at + 2..];
        let end = suffix.find("}}").ok_or(ContextError::Invalid)?;
        let value = match suffix[..end].trim() {
            "assistant.role" => {
                if role == AgentRole::Main {
                    "main"
                } else {
                    "worker"
                }
            }
            "channel.constraints" => channel,
            _ => return Err(ContextError::Invalid),
        };
        result.push_str(value);
        rest = &suffix[end + 2..];
    }
    result.push_str(rest);
    Ok(result)
}

pub(crate) const MAIN_RULES: &str = include_str!("../../prompts/main.md");
pub(crate) const WORKER_RULES: &str = include_str!("../../prompts/worker.md");

#[cfg(test)]
#[path = "assembly_test.rs"]
mod tests;
