//! Merge explicit account/operator skill roots with a device-local project catalog.
use super::*;
use crate::context::ContextFiles;
use peri_acp_types::device_executor::WorkspaceFile;
use uuid::Uuid;

struct Reader {
    device: Option<Arc<dyn SkillReader>>,
    files: ContextFiles,
    principal: Uuid,
    private: bool,
}

#[async_trait]
impl SkillReader for Reader {
    async fn read(&self, path: &str) -> Result<WorkspaceFile, String> {
        if path.starts_with("account/") || path.starts_with("operator/") {
            return self
                .files
                .read_global_skill(self.principal, self.private, path)
                .await
                .map_err(|_| "authorized global skill read failed".into());
        }
        self.device
            .as_ref()
            .ok_or("device skill is unavailable")?
            .read(path)
            .await
    }
}
pub(crate) async fn with_global_skills(
    workspace: Option<Arc<DeviceWorkspace>>,
    files: Option<&ContextFiles>,
    principal: Uuid,
    private: bool,
) -> Option<Arc<DeviceWorkspace>> {
    let Some(files) = files else {
        return workspace;
    };
    let globals = files
        .global_skills(principal, private)
        .await
        .unwrap_or_default();
    if globals.is_empty() {
        return workspace;
    }
    let mut skills = workspace
        .as_ref()
        .map(|w| w.skills.clone())
        .unwrap_or_default();
    // Project > account > operator. Explicit catalog paths always identify origin.
    let mut names: std::collections::HashSet<_> =
        skills.iter().map(|s| s.name.to_lowercase()).collect();
    let mut globals = globals;
    globals.sort_by_key(|s| s.source != "account");
    for item in globals {
        let name = if names.insert(item.name.to_lowercase()) {
            item.name.clone()
        } else {
            format!("{}:{}", item.source, item.name)
        };
        skills.push(WorkspaceSkill {
            name,
            path: item.path,
            source: if item.source == "account" {
                SkillSource::User
            } else {
                SkillSource::Builtin
            },
        });
    }
    Some(Arc::new(DeviceWorkspace {
        main: workspace.as_ref().and_then(|w| w.main.clone()),
        local: workspace.as_ref().and_then(|w| w.local.clone()),
        environment: workspace.as_ref().and_then(|w| w.environment.clone()),
        skills,
        skills_available: true,
        reader: Arc::new(Reader {
            device: workspace.as_ref().map(|w| w.reader.clone()),
            files: files.clone(),
            principal,
            private,
        }),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn global_skills_work_without_device_and_project_names_take_precedence() {
        let root = tempfile::tempdir().unwrap();
        let principal = Uuid::new_v4();
        for (file, body) in [
            (
                format!("users/{principal}/skills/review/SKILL.md"),
                "account marker",
            ),
            ("skills/review/SKILL.md".into(), "operator marker"),
        ] {
            let path = root.path().join(file);
            tokio::fs::create_dir_all(path.parent().unwrap())
                .await
                .unwrap();
            tokio::fs::write(path, body).await.unwrap();
        }
        let files = ContextFiles::new(root.path().into());
        let global = with_global_skills(None, Some(&files), principal, true)
            .await
            .unwrap();
        assert_eq!(global.skills[0].name, "review");
        assert!(global.skills.iter().any(|s| s.name == "operator:review"));
        let project = Arc::new(DeviceWorkspace {
            main: None,
            local: None,
            environment: None,
            skills: super::super::catalog([".claude/skills/review/SKILL.md"]),
            skills_available: true,
            reader: global.reader.clone(),
        });
        let merged = with_global_skills(Some(project), Some(&files), principal, true)
            .await
            .unwrap();
        assert!(merged
            .skills
            .iter()
            .any(|s| s.name == "review" && s.source == SkillSource::Project));
        assert!(merged.skills.iter().any(|s| s.name == "account:review"));
        assert_eq!(
            merged
                .reader
                .read("account/skills/review/SKILL.md")
                .await
                .unwrap()
                .content,
            "account marker"
        );
        let public = with_global_skills(None, Some(&files), principal, false)
            .await
            .unwrap();
        assert!(public.skills.iter().all(|s| s.source != SkillSource::User));
    }
}
