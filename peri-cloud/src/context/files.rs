use super::ContextError;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

const MAX_FILE: u64 = 32768;
#[derive(Clone)]
pub struct ContextFiles {
    pub(super) root: PathBuf,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextDocument {
    pub name: String,
    pub text: String,
}

pub(super) fn valid_relative(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && !value.contains(['\\', ':', '\0'])
        && Path::new(value)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        && value.ends_with(".md")
}
impl ContextFiles {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    pub(super) async fn read(&self, name: &str) -> Result<Option<String>, ContextError> {
        self.read_scoped(name, None).await
    }
    pub(super) async fn read_scoped(
        &self,
        name: &str,
        scope: Option<&str>,
    ) -> Result<Option<String>, ContextError> {
        if !valid_relative(name) {
            return Err(ContextError::Invalid);
        }
        let target = self.root.join(name);
        let canonical = match tokio::fs::canonicalize(&target).await {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(ContextError::File),
        };
        let root = tokio::fs::canonicalize(&self.root)
            .await
            .map_err(|_| ContextError::File)?;
        if !canonical.starts_with(&root) {
            return Err(ContextError::Invalid);
        }
        if scope.is_some_and(|scope| !canonical.starts_with(root.join(scope))) {
            return Err(ContextError::Invalid);
        }
        let metadata = tokio::fs::metadata(&canonical)
            .await
            .map_err(|_| ContextError::File)?;
        if !metadata.is_file() || metadata.len() > MAX_FILE {
            return Err(ContextError::Limit);
        }
        let bytes = tokio::fs::read(&canonical)
            .await
            .map_err(|_| ContextError::File)?;
        if bytes.len() > MAX_FILE as usize {
            return Err(ContextError::Limit);
        }
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| ContextError::Invalid)
    }
    pub async fn read_operator(&self, name: &str) -> Result<Option<String>, ContextError> {
        self.read(name).await
    }
    pub async fn read_user(
        &self,
        principal: Uuid,
        name: &str,
    ) -> Result<Option<String>, ContextError> {
        if name != "SOUL.md" {
            return Err(ContextError::Invalid);
        }
        if let Ok(path) =
            tokio::fs::canonicalize(self.root.join(format!("users/{principal}/{name}"))).await
        {
            let root = tokio::fs::canonicalize(&self.root)
                .await
                .map_err(|_| ContextError::File)?;
            if !path.starts_with(root.join("users").join(principal.to_string())) {
                return Err(ContextError::Invalid);
            }
        }
        self.read(&format!("users/{principal}/{name}")).await
    }
    /// Called by authenticated owner management only; no path is accepted from a model tool.
    pub async fn write_user(
        &self,
        principal: Uuid,
        document: ContextDocument,
    ) -> Result<(), ContextError> {
        if document.name != "SOUL.md" {
            return Err(ContextError::Invalid);
        }
        if document.text.len() > MAX_FILE as usize {
            return Err(ContextError::Limit);
        }
        tokio::fs::create_dir_all(&self.root)
            .await
            .map_err(|_| ContextError::File)?;
        let root = tokio::fs::canonicalize(&self.root)
            .await
            .map_err(|_| ContextError::File)?;
        let mut directory = root.clone();
        for component in ["users".to_owned(), principal.to_string()] {
            directory = directory.join(component);
            if let Ok(existing) = tokio::fs::canonicalize(&directory).await {
                if existing != directory {
                    return Err(ContextError::Invalid);
                }
            } else {
                tokio::fs::create_dir(&directory)
                    .await
                    .map_err(|_| ContextError::File)?;
            }
        }
        let canonical = tokio::fs::canonicalize(&directory)
            .await
            .map_err(|_| ContextError::File)?;
        if !canonical.starts_with(&root) {
            return Err(ContextError::Invalid);
        }
        let target = directory.join("SOUL.md");
        if tokio::fs::symlink_metadata(&target)
            .await
            .is_ok_and(|meta| meta.file_type().is_symlink())
        {
            return Err(ContextError::Invalid);
        }
        let temporary = directory.join(format!(".{}.tmp", Uuid::new_v4()));
        tokio::fs::write(&temporary, document.text)
            .await
            .map_err(|_| ContextError::File)?;
        tokio::fs::rename(&temporary, target)
            .await
            .map_err(|_| ContextError::File)
    }
}
