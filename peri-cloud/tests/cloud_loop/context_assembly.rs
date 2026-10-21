use super::*;

#[tokio::test]
async fn cwd_rules_environment_and_skill_catalog_reach_model_without_project_soul() {
    let h = Harness::new(vec![answer("context loaded")], false).await;
    std::fs::write(
        h.workspace.path().join("AGENTS.md"),
        "PROJECT_AGENTS_MARKER",
    )
    .unwrap();
    std::fs::write(
        h.workspace.path().join("CLAUDE.md"),
        "UNSELECTED_CLAUDE_MARKER",
    )
    .unwrap();
    std::fs::write(
        h.workspace.path().join("SOUL.md"),
        "UNAUTHORIZED_PROJECT_SOUL_MARKER",
    )
    .unwrap();
    std::fs::write(
        h.workspace.path().join("CLAUDE.local.md"),
        "PROJECT_LOCAL_MARKER",
    )
    .unwrap();
    let skill = h.workspace.path().join(".agents/skills/review/SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(skill, "DO_NOT_LOAD_ENTIRE_SKILL_BODY_MARKER").unwrap();
    assert_eq!(
        h.run(PermissionMode::Default, Vec::new()).await.status,
        TurnStatus::Completed
    );
    let request = h.script.requests.lock()[0].to_string();
    assert!(request.contains("PROJECT_AGENTS_MARKER"));
    assert!(request.contains("PROJECT_LOCAL_MARKER"));
    assert!(!request.contains("UNSELECTED_CLAUDE_MARKER"));
    assert!(!request.contains("UNAUTHORIZED_PROJECT_SOUL_MARKER"));
    assert!(request.contains("Target device environment"));
    assert!(request.contains("architecture"));
    assert!(request.contains("review"));
    assert!(!request.contains("DO_NOT_LOAD_ENTIRE_SKILL_BODY_MARKER"));
    assert!(!request.contains(&h.transport_token));
    h.close().await;
}
