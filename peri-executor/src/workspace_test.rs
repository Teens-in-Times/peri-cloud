use std::fs;
use std::path::Path;

fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[test]
fn instructions_prefer_the_first_non_blank_candidate() {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "AGENTS.md", "  \n\t\n");
    write(root.path(), "CLAUDE.md", "claude body");
    write(root.path(), "CLAUDE.local.md", "local body");

    let instructions = super::instructions(root.path().to_str().unwrap()).unwrap();
    let main = instructions.main.expect("CLAUDE.md fallback");
    assert_eq!(main.path, "CLAUDE.md");
    assert_eq!(main.content, "claude body");
    assert!(main.digest.starts_with("sha256:"));
    assert_eq!(main.digest.len(), 7 + 64);
    assert_eq!(instructions.local.unwrap().content, "local body");

    write(root.path(), "AGENTS.md", "agents body");
    let instructions = super::instructions(root.path().to_str().unwrap()).unwrap();
    assert_eq!(instructions.main.unwrap().path, "AGENTS.md");
}

#[test]
fn instructions_fall_back_when_the_first_candidate_is_oversized() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "AGENTS.md",
        &"x".repeat(super::MAX_INSTRUCTION_BYTES as usize + 1),
    );
    write(root.path(), "CLAUDE.md", "claude body");
    let instructions = super::instructions(root.path().to_str().unwrap()).unwrap();
    assert_eq!(instructions.main.unwrap().path, "CLAUDE.md");
}

#[test]
fn skills_only_lists_project_skill_entries() {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), ".claude/skills/review/SKILL.md", "a");
    write(root.path(), ".claude/skills/team/deploy/SKILL.md", "b");
    write(
        root.path(),
        ".claude/skills/review/README.md",
        "not an entry",
    );
    write(root.path(), "elsewhere/SKILL.md", "outside the root");

    let skills = super::skills(root.path().to_str().unwrap());
    assert_eq!(
        skills.paths,
        vec![
            ".claude/skills/review/SKILL.md".to_string(),
            ".claude/skills/team/deploy/SKILL.md".to_string(),
        ]
    );
    assert!(!skills.truncated);
}

#[test]
fn read_skill_only_accepts_the_discovered_shape() {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), ".claude/skills/review/SKILL.md", "skill body");
    let workspace = root.path().to_str().unwrap();

    let file = super::read_skill(workspace, ".claude/skills/review/SKILL.md").unwrap();
    assert_eq!(file.content, "skill body");
    assert!(file.digest.starts_with("sha256:"));

    for invalid in [
        "../SKILL.md",
        ".claude/skills/../SKILL.md",
        ".claude/skills/review/README.md",
        "/etc/SKILL.md",
        ".claude/skills/SKILL.md",
        ".claude\\skills\\review\\SKILL.md",
    ] {
        assert!(
            super::read_skill(workspace, invalid).is_err(),
            "{invalid} must be rejected"
        );
    }
}

#[test]
fn a_missing_skill_is_not_found() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().to_str().unwrap();
    assert!(matches!(
        super::read_skill(workspace, ".claude/skills/missing/SKILL.md"),
        Err(crate::Error::NotFound)
    ));
}

#[cfg(unix)]
#[test]
fn symlinked_skills_are_not_followed() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(outside.path(), "SKILL.md", "outside body");
    fs::create_dir_all(root.path().join(".claude/skills")).unwrap();
    symlink(outside.path(), root.path().join(".claude/skills/linked")).unwrap();
    let workspace = root.path().to_str().unwrap();

    assert!(
        super::skills(workspace).paths.is_empty(),
        "symlinked skill directories must not be scanned"
    );
    assert!(matches!(
        super::read_skill(workspace, ".claude/skills/linked/SKILL.md"),
        Err(crate::Error::NotFound)
    ));
}
