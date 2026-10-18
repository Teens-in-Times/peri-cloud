use std::sync::Arc;

use async_trait::async_trait;
use peri_acp_types::device_executor::WorkspaceFile;
use peri_agent::middleware::r#trait::Middleware;
use peri_agent::tools::ToolContext;
use serde_json::json;

use super::*;
use crate::workspace::{DeviceWorkspace, SkillReader, WorkspaceSkill};

struct FakeReader;

#[async_trait]
impl SkillReader for FakeReader {
    async fn read(&self, path: &str) -> Result<WorkspaceFile, String> {
        Ok(WorkspaceFile {
            path: path.to_owned(),
            content: format!("full body of {path}"),
            digest: "sha256:fixture".into(),
        })
    }
}

fn workspace(skills: Vec<WorkspaceSkill>) -> Arc<DeviceWorkspace> {
    Arc::new(DeviceWorkspace {
        main: None,
        local: None,
        skills,
        skills_available: true,
        reader: Arc::new(FakeReader),
    })
}

fn skill(name: &str, path: &str) -> WorkspaceSkill {
    WorkspaceSkill {
        name: name.into(),
        path: path.into(),
    }
}

#[tokio::test]
async fn skill_tool_loads_by_name_or_path_and_reports_the_digest() {
    let workspace = workspace(vec![
        skill("review", ".claude/skills/review/SKILL.md"),
        skill("deploy", ".claude/skills/team/deploy/SKILL.md"),
    ]);
    let tool = SkillTool {
        workspace: workspace.clone(),
    };
    let messages = Vec::new();
    let ctx = ToolContext::new(&messages, "");

    let output = tool
        .invoke(json!({"skill_name": "REVIEW"}), ctx)
        .await
        .unwrap();
    assert!(output.contains("full body of .claude/skills/review/SKILL.md"));
    assert!(output.contains("sha256:fixture"));

    let ctx = ToolContext::new(&messages, "");
    let output = tool
        .invoke(
            json!({"skill_name": ".claude/skills/team/deploy/SKILL.md"}),
            ctx,
        )
        .await
        .unwrap();
    assert!(output.contains("full body of .claude/skills/team/deploy/SKILL.md"));
}

#[tokio::test]
async fn skill_tool_rejects_unknown_names() {
    let tool = SkillTool {
        workspace: workspace(vec![skill("review", ".claude/skills/review/SKILL.md")]),
    };
    let messages = Vec::new();
    let error = tool
        .invoke(
            json!({"skill_name": "missing"}),
            ToolContext::new(&messages, ""),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("was not found"));
}

#[test]
fn ambiguous_names_require_a_path() {
    let skills = vec![
        skill("review", ".claude/skills/a/review/SKILL.md"),
        skill("review", ".claude/skills/b/review/SKILL.md"),
    ];
    let error = find_skill(&skills, "review").unwrap_err();
    assert!(error.contains("ambiguous"));
}

#[tokio::test]
async fn discovery_returns_the_catalog_and_filters_by_query() {
    let workspace = workspace(vec![
        skill("review", ".claude/skills/review/SKILL.md"),
        skill("deploy", ".claude/skills/team/deploy/SKILL.md"),
    ]);
    let tool = DiscoverSkillsTool {
        workspace: workspace.clone(),
    };
    let messages = Vec::new();

    let output = tool
        .invoke(json!({}), ToolContext::new(&messages, ""))
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed.as_array().unwrap().len(), 2);

    let output = tool
        .invoke(json!({"query": "DEP"}), ToolContext::new(&messages, ""))
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed.as_array().unwrap().len(), 1);
    assert_eq!(parsed[0]["name"], "deploy");
    assert_eq!(parsed[0]["path"], ".claude/skills/team/deploy/SKILL.md");
}

#[tokio::test]
async fn middleware_exposes_the_catalog_and_the_cloud_section() {
    let middleware = CloudSkillsMiddleware::new(workspace(vec![skill(
        "review",
        ".claude/skills/review/SKILL.md",
    )]));
    let names: Vec<_> = middleware
        .collect_tools("")
        .into_iter()
        .map(|tool| tool.name().to_owned())
        .collect();
    assert_eq!(names, vec!["SkillTool", "DiscoverSkillsTool"]);
    assert!(middleware
        .prompt_contribution()
        .unwrap()
        .contains("**review**"));
    let sections = middleware.prompt_sections();
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].id, "13_skills");
    assert!(sections[0]
        .content
        .as_str()
        .contains("bound device workspace"));
}
