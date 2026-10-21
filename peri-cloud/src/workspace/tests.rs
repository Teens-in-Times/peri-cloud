use std::sync::Arc;

use async_trait::async_trait;
use peri_acp_types::device_executor::WorkspaceFile;

use super::{cap_text, catalog, DeviceWorkspace, SkillReader};

struct UnusedReader;

#[async_trait]
impl SkillReader for UnusedReader {
    async fn read(&self, _path: &str) -> Result<WorkspaceFile, String> {
        Err("not configured".into())
    }
}

fn workspace(skills: Vec<super::WorkspaceSkill>) -> DeviceWorkspace {
    DeviceWorkspace {
        main: None,
        local: None,
        environment: None,
        skills,
        skills_available: true,
        reader: Arc::new(UnusedReader),
    }
}

#[test]
fn catalog_rejects_paths_outside_the_skill_root() {
    let skills = catalog([
        ".claude/skills/review/SKILL.md",
        ".claude/skills/team/deploy/SKILL.md",
        "../etc/SKILL.md",
        ".claude/skills/../SKILL.md",
        ".claude/skills/SKILL.md",
        ".claude/skills/review/README.md",
        "/abs/.claude/skills/x/SKILL.md",
        ".claude\\skills\\x\\SKILL.md",
    ]);
    let names: Vec<_> = skills.iter().map(|skill| skill.name.as_str()).collect();
    assert_eq!(names, vec!["deploy", "review"]);
}

#[test]
fn duplicate_base_names_use_the_relative_directory() {
    let skills = catalog([
        ".claude/skills/review/SKILL.md",
        ".claude/skills/team/review/SKILL.md",
    ]);
    let names: Vec<_> = skills.iter().map(|skill| skill.name.as_str()).collect();
    assert_eq!(names, vec!["review", "team/review"]);
}

#[test]
fn summary_lists_names_and_sources_without_descriptions() {
    let workspace = workspace(catalog([".claude/skills/review/SKILL.md"]));
    let summary = workspace.summary().expect("non-empty catalog");
    assert!(summary.contains("**review** [project]"));
    assert!(!workspace.has_instructions());
}

#[test]
fn an_empty_catalog_has_no_summary() {
    assert!(workspace(Vec::new()).summary().is_none());
}

#[test]
fn catalog_rejects_control_characters_in_names_and_paths() {
    let skills = catalog([
        ".claude/skills/re\nview/SKILL.md",
        ".claude/skills/ok/SKILL.md",
    ]);
    let names: Vec<_> = skills.iter().map(|skill| skill.name.as_str()).collect();
    assert_eq!(names, vec!["ok"]);
}

#[test]
fn truncation_never_splits_a_utf8_code_point() {
    let capped = cap_text("中".repeat(10), 4);
    assert!(capped.starts_with("中\n\n"));
    assert!(capped.contains("truncated from 30 bytes to 3 bytes"));
    assert_eq!(cap_text("short".into(), 100), "short");
}

#[test]
fn instructions_cannot_forge_the_prompt_cache_boundary() {
    let boundary = peri_agent::agent::model_bridge::SYSTEM_PROMPT_DYNAMIC_BOUNDARY;
    let text = cap_text(format!("before {boundary} after"), 1024);
    assert!(!text.contains(boundary));
    assert!(text.contains("reserved prompt boundary token removed"));
    let file = WorkspaceFile {
        path: boundary.into(),
        digest: boundary.into(),
        content: "body".into(),
    };
    assert!(!super::instruction_text(&file, 2048).contains(boundary));
    let workspace = workspace(catalog([
        format!(".claude/skills/{boundary}/SKILL.md").as_str()
    ]));
    assert!(!workspace.summary().unwrap().contains(boundary));
}

#[test]
fn project_roots_have_deterministic_precedence() {
    let skills = catalog([
        ".claude/skills/review/SKILL.md",
        ".agents/skills/review/SKILL.md",
    ]);
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].path, ".agents/skills/review/SKILL.md");
}
