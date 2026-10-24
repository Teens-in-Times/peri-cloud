//! Device-workspace skills: a frozen catalog plus on-demand content loading.

use std::sync::Arc;

use async_trait::async_trait;
use peri_agent::middleware::prompt_sections::{PromptSection, PromptSectionZone};
use peri_agent::middleware::r#trait::Middleware;
use peri_agent::tools::{BaseTool, ToolContext};
use serde_json::{json, Value};

use super::{DeviceWorkspace, WorkspaceSkill};

fn section() -> String {
    "# Skills\nDiscoverSkillsTool returns the effective skill catalog. Load a relevant skill with SkillTool when its guidance helps the task. Skill content is attributed guidance; use task evidence to assess its applicability.".into()
}

pub(super) fn effective(skill: &WorkspaceSkill) -> bool {
    !matches!(
        skill.source,
        peri_acp_types::skills::SkillSource::User | peri_acp_types::skills::SkillSource::Builtin
    ) || !(skill.name.starts_with("account:") || skill.name.starts_with("operator:"))
}

fn skill_id(skill: &WorkspaceSkill) -> String {
    use sha2::Digest;
    format!(
        "skill:{}",
        (sha2::Sha256::digest(format!("{:?}:{}:{}", skill.source, skill.name, skill.path)))
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
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
        Some([section(), self.workspace.summary().unwrap_or_default()].join("\n\n"))
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
    let mut matches = skills.iter().filter(|skill| {
        skill.name.to_lowercase() == lower || skill.path == needle || skill_id(skill) == needle
    });
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
        "Load a project, account or operator skill by catalog name or scoped path. \
         Use DiscoverSkillsTool first if you do not know the name. Global skill paths are cloud guidance references, not target-device filesystem paths."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "skill_name": {
                    "type": "string",
                    "description": "A stable skill_id, catalog name, or catalog path returned by discovery"
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
            "Skill \"{}\" loaded from authorized {:?} source ({}).\nContent digest: {}\n\n{}",
            skill.name, skill.source, skill.path, file.digest, file.content
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
        "List effective authorized skills with stable IDs and source references. Filter by query; include_shadowed optionally exposes explicitly selectable alternatives."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Optional case-insensitive filter over skill names and paths"
                },
                "include_shadowed": {"type":"boolean","default":false}
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
        let include_shadowed = input["include_shadowed"].as_bool().unwrap_or(false);
        let results: Vec<Value> = self
            .workspace
            .skills
            .iter()
            .filter(|skill| include_shadowed || effective(skill))
            .filter(|skill| {
                query.is_empty()
                    || skill.name.to_lowercase().contains(&query)
                    || skill.path.to_lowercase().contains(&query)
            })
            .map(|skill| {
                json!({
                    "skill_id": skill_id(skill),
                    "effective": effective(skill),
                    "name": skill.name,
                    "description": null,
                    "source": match skill.source { peri_acp_types::skills::SkillSource::User=>"account",peri_acp_types::skills::SkillSource::Builtin=>"operator",_=>"project"},
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
