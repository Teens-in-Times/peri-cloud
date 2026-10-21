use super::*;

#[test]
fn sections_render_host_macros_once_and_enforce_budgets() {
    let mut config = ContextConfig::default();
    config.section_budgets.insert("role".into(), 100);
    let assembler = ContextAssembler::new(config, std::env::temp_dir()).unwrap();
    let parts = vec![
        ContextPart::text("base", "Role={{assistant.role}}; {{channel.constraints}}"),
        ContextPart::text("channel", "literal {{untrusted}}"),
        ContextPart::text("role", "worker rules"),
    ];
    let value = assembler
        .assemble_for_role(parts, AgentRole::Worker)
        .unwrap();
    assert!(value
        .system_prompt
        .contains("Role=worker; literal {{untrusted}}"));
    assert_eq!(value.sections[0].role, "worker");
    assert_eq!(value.sections[0].version.len(), 71);
    assert!(assembler
        .assemble(vec![ContextPart::text("base", "{{unknown}}")])
        .is_err());
    assert!(assembler
        .assemble(vec![ContextPart::text("role", "x".repeat(101))])
        .is_err());
    assert!(assembler
        .assemble(vec![ContextPart::text(
            "base",
            peri_model::prompt_cache::SYSTEM_PROMPT_DYNAMIC_BOUNDARY
        )])
        .is_err());
}

#[tokio::test]
async fn global_skill_reads_are_account_private_and_path_bounded() {
    let root = tempfile::tempdir().unwrap();
    let user = Uuid::new_v4();
    let other = Uuid::new_v4();
    for (relative, body) in [
        (
            format!("users/{user}/skills/review/SKILL.md"),
            "private body",
        ),
        ("skills/shared/SKILL.md".into(), "operator body"),
    ] {
        let file = root.path().join(relative);
        tokio::fs::create_dir_all(file.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(file, body).await.unwrap();
    }
    let files = ContextFiles::new(root.path().into());
    assert_eq!(files.global_skills(user, true).await.unwrap().len(), 2);
    assert_eq!(files.global_skills(user, false).await.unwrap().len(), 1);
    assert_eq!(files.global_skills(other, true).await.unwrap().len(), 1);
    assert_eq!(
        files
            .read_global_skill(user, true, "account/skills/review/SKILL.md")
            .await
            .unwrap()
            .content,
        "private body"
    );
    for (principal, private, path) in [
        (user, false, "account/skills/review/SKILL.md"),
        (other, true, "account/skills/review/SKILL.md"),
        (user, true, "account/skills/../../skills/shared/SKILL.md"),
    ] {
        assert!(files
            .read_global_skill(principal, private, path)
            .await
            .is_err());
    }
}
