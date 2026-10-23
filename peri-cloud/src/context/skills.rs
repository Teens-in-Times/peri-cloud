//! Explicit operator/account skill roots, never the cloud machine's home directory.
use super::{ContextError, ContextFiles};
use peri_acp_types::device_executor::WorkspaceFile;
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub(crate) struct GlobalSkill {
    pub path: String,
    pub name: String,
    pub source: String,
}
impl ContextFiles {
    pub(crate) async fn global_skills(
        &self,
        principal: Uuid,
        private: bool,
    ) -> Result<Vec<GlobalSkill>, ContextError> {
        let mut result = Vec::new();
        let roots = if private {
            vec![
                (format!("users/{principal}/skills"), "account"),
                ("skills".into(), "operator"),
            ]
        } else {
            vec![("skills".into(), "operator")]
        };
        for (relative, scope) in roots {
            let mut stack = vec![(relative.clone(), 0)];
            let mut visited = 0;
            while let Some((directory, depth)) = stack.pop() {
                visited += 1;
                if visited > 512 || result.len() >= 128 {
                    break;
                }
                if depth > 6 {
                    continue;
                }
                let absolute = self.root.join(&directory);
                // No symlink traversal, including the account and skills root components.
                if !plain_directory(&self.root, &directory).await {
                    continue;
                }
                let Ok(mut entries) = tokio::fs::read_dir(&absolute).await else {
                    continue;
                };
                let mut names = Vec::new();
                while let Ok(Some(entry)) = entries.next_entry().await {
                    names.push(entry.file_name());
                    if names.len() >= 512 {
                        break;
                    }
                }
                names.sort();
                for name in names {
                    let Some(name) = name.to_str() else {
                        continue;
                    };
                    if name.chars().any(char::is_control) {
                        continue;
                    }
                    let child = format!("{directory}/{name}");
                    if child.len() > 200 || name.contains(['\\', ':', '\0']) {
                        continue;
                    }
                    let Ok(metadata) = tokio::fs::symlink_metadata(self.root.join(&child)).await
                    else {
                        continue;
                    };
                    if metadata.file_type().is_symlink() {
                        continue;
                    }
                    if metadata.is_dir() {
                        stack.push((child, depth + 1));
                    } else if name == "SKILL.md" && metadata.is_file() && result.len() < 128 {
                        let Some(tail) = child.strip_prefix(&(relative.clone() + "/")) else {
                            continue;
                        };
                        let Some(dir) = tail.strip_suffix("/SKILL.md") else {
                            continue;
                        };
                        if dir.is_empty() {
                            continue;
                        }
                        if dir.chars().count() > 120 {
                            continue;
                        }
                        result.push(GlobalSkill {
                            path: format!("{scope}/skills/{tail}"),
                            name: dir.to_owned(),
                            source: scope.into(),
                        });
                    }
                }
            }
        }
        result.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(result)
    }
    pub(crate) async fn read_global_skill(
        &self,
        principal: Uuid,
        private: bool,
        path: &str,
    ) -> Result<WorkspaceFile, ContextError> {
        let relative = if let Some(tail) = path.strip_prefix("account/skills/") {
            if !private {
                return Err(ContextError::Invalid);
            }
            format!("users/{principal}/skills/{tail}")
        } else if let Some(tail) = path.strip_prefix("operator/skills/") {
            format!("skills/{tail}")
        } else {
            return Err(ContextError::Invalid);
        };
        if !relative.ends_with("/SKILL.md") || !super::files::valid_relative(&relative) {
            return Err(ContextError::Invalid);
        }
        let parent = relative.rsplit_once('/').ok_or(ContextError::Invalid)?.0;
        if !plain_directory(&self.root, parent).await {
            return Err(ContextError::Invalid);
        }
        if tokio::fs::symlink_metadata(self.root.join(&relative))
            .await
            .is_ok_and(|m| m.file_type().is_symlink())
        {
            return Err(ContextError::Invalid);
        }
        let scope = if relative.starts_with("users/") {
            format!("users/{principal}/skills")
        } else {
            "skills".into()
        };
        let content = self
            .read_scoped(&relative, Some(&scope))
            .await?
            .ok_or(ContextError::File)?;
        let digest = format!(
            "sha256:{}",
            (Sha256::digest(content.as_bytes()))
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        Ok(WorkspaceFile {
            path: path.into(),
            content,
            digest,
        })
    }
}
async fn plain_directory(root: &std::path::Path, relative: &str) -> bool {
    if !relative
        .split('/')
        .all(|s| !s.is_empty() && s != "." && s != ".." && !s.contains(['\\', ':']))
    {
        return false;
    }
    let mut current = root.to_owned();
    for component in relative.split('/') {
        current.push(component);
        if !tokio::fs::symlink_metadata(&current)
            .await
            .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
        {
            return false;
        }
    }
    true
}
