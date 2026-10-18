//! Device-workspace skills: a frozen catalog plus on-demand content loading.

use std::sync::Arc;

use async_trait::async_trait;
use peri_agent::middleware::prompt_sections::{PromptSection, PromptSectionZone};
use peri_agent::middleware::r#trait::Middleware;
use peri_agent::tools::{BaseTool, ToolContext};
use serde_json::{json, Value};

use super::{DeviceWorkspace, WorkspaceSkill};

fn section() -> String {
    [
        "# Skills",
        "",
        "Skills are instructions stored in `SKILL.md` files under the bound device \
         workspace's `.claude/skills` directory.",
        "",
        "## Loading protocol",
        "",
        "- `DiscoverSkillsTool(query?)` returns this session's skill catalog \
         (name, source, workspace-relative path).",
        "- `SkillTool(skill_name)` loads a skill's full `SKILL.md` by name or path.",
        "",
        "The catalog is frozen when the session starts; `SkillTool` reads the current \
         file from the same device workspace on demand. Loading a skill never grants \
         permissions: commands, file writes and network actions still go through the \
         normal tools and approval policy. Scripts and other resources referenced by a \
         skill must be read or executed with the regular device tools and remain subject \
         to workspace and permission checks.",
    ]
    .join("\n")
}

pub(crate) struct CloudSkillsMiddleware {
    workspace: Arc<DeviceWorkspace>,
}

impl CloudSkillsMiddleware {
    pub(crate) fn new(workspace: Arc<DeviceWorkspace>) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Middleware for CloudSkillsMiddleware {
    fn name(&self) -> &str {
        "SkillsMiddleware"
    }

    fn prompt_sections(&self) -> Vec<PromptSection> {
        vec![PromptSection::dynamic(
            "13_skills",
            PromptSectionZone::Uncached,
            6,
            section(),
        )]
    }

    fn prompt_contribution(&self) -> Option<String> {
        self.workspace.summary()
    }

    fn collect_tools(&self, _cwd: &str) -> Vec<Box<dyn BaseTool>> {
        vec![
            Box::new(SkillTool {
                workspace: self.workspace.clone(),
            }),
            Box::new(DiscoverSkillsTool {
                workspace: self.workspace.clone(),
            }),
        ]
    }
}

fn find_skill<'a>(skills: &'a [WorkspaceSkill], name: &str) -> Result<&'a WorkspaceSkill, String> {
    let needle = name.trim();
    if needle.is_empty() {
        return Err("SkillTool: 'skill_name' cannot be empty".to_owned());
    }
    let lower = needle.to_lowercase();
    let mut matches = skills
        .iter()
        .filter(|skill| skill.name.to_lowercase() == lower || skill.path == needle);
    match (matches.next(), matches.next()) {
        (Some(skill), None) => Ok(skill),
        (None, _) => Err(format!(
            "SkillTool: skill '{needle}' was not found; use DiscoverSkillsTool to see the catalog"
        )),
        (Some(_), Some(_)) => Err(format!(
            "SkillTool: skill name '{needle}' is ambiguous; use the workspace-relative path"
        )),
    }
}

struct SkillTool {
    workspace: Arc<DeviceWorkspace>,
}

#[async_trait]
impl BaseTool for SkillTool {
    fn name(&self) -> &str {
        "SkillTool"
    }

    fn description(&self) -> &str {
        "Load the full content of a skill from the bound device workspace by name or \
         workspace-relative path. Use DiscoverSkillsTool first if you do not know the name."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "skill_name": {
                    "type": "string",
                    "description": "A skill name from the catalog, or its workspace-relative SKILL.md path"
                }
            },
            "required": ["skill_name"]
        })
    }

    fn is_direct(&self) -> bool {
        true
    }

    fn namespace(&self) -> Option<&str> {
        Some("skills")
    }

    fn output_char_limit(&self) -> Option<usize> {
        Some(128_000)
    }

    fn prompt_declaration(&self) -> Option<String> {
        Some(
            "Load a skill's full SKILL.md from the bound device workspace → `{{name}}` \
             ({{title}}), by catalog name or path."
                .to_string(),
        )
    }

    async fn invoke(
        &self,
        input: Value,
        _ctx: ToolContext<'_>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let name = input["skill_name"]
            .as_str()
            .ok_or("SkillTool: missing required parameter 'skill_name'")?;
        let skill = find_skill(&self.workspace.skills, name)?;
        let file = self.workspace.reader.read(&skill.path).await.map_err(
            |error| -> Box<dyn std::error::Error + Send + Sync> {
                format!("SkillTool: {error}").into()
            },
        )?;
        Ok(format!(
            "Skill \"{}\" loaded from the bound device workspace ({}).\nContent digest: {}\n\n{}",
            skill.name, skill.path, file.digest, file.content
        ))
    }
}

struct DiscoverSkillsTool {
    workspace: Arc<DeviceWorkspace>,
}

#[async_trait]
impl BaseTool for DiscoverSkillsTool {
    fn name(&self) -> &str {
        "DiscoverSkillsTool"
    }

    fn description(&self) -> &str {
        "List the bound device workspace's skill catalog (name, source and \
         workspace-relative path). An optional query filters by name or path. Returns \
         an empty array when the workspace has no skills."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Optional case-insensitive filter over skill names and paths"
                }
            }
        })
    }

    fn is_direct(&self) -> bool {
        true
    }

    fn namespace(&self) -> Option<&str> {
        Some("skills")
    }

    fn prompt_declaration(&self) -> Option<String> {
        Some(
            "List the session's skills from the bound device workspace → `{{name}}` \
             ({{title}}), optionally filtered by a name/path query."
                .to_string(),
        )
    }

    async fn invoke(
        &self,
        input: Value,
        _ctx: ToolContext<'_>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let query = input["query"].as_str().unwrap_or("").to_lowercase();
        let results: Vec<Value> = self
            .workspace
            .skills
            .iter()
            .filter(|skill| {
                query.is_empty()
                    || skill.name.to_lowercase().contains(&query)
                    || skill.path.to_lowercase().contains(&query)
            })
            .map(|skill| {
                json!({
                    "name": skill.name,
                    "description": null,
                    "source": "project",
                    "path": skill.path,
                })
            })
            .collect();
        Ok(serde_json::to_string(&results)?)
    }
}

#[cfg(test)]
#[path = "skills_test.rs"]
mod tests;
