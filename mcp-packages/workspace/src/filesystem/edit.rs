//! Edit 封装层（peri_agent::tools::BaseTool）。
//!
//! 编辑应用核心（读 → sha 护栏 → 定位/替换 → 哨兵 → 原子提交）在
//! `peri-tool-runtime::filesystem::edit::apply_edit` 单一实现（P0-1b），
//! 本文件只做参数校验（P0-4 new==old）与 ToolFailure/path 提示映射。

use peri_agent::tools::BaseTool;
use peri_mcp_common::failure::ToolFailure;
use serde_json::Value;

use super::resolve_path;
#[cfg(test)]
pub(crate) use peri_tool_runtime::filesystem::edit::build_not_found_hint;
use peri_tool_runtime::filesystem::edit::{apply_edit, EditCoreError};
pub(crate) use peri_tool_runtime::filesystem::transaction::SENTINEL_REJECTION;
use peri_tool_runtime::filesystem::write::{parse_expected_sha256, SHA_MISMATCH_REJECTION};

const EDIT_FILE_DESCRIPTION: &str = include_str!("descriptions/edit.md");

/// Edit tool (replace) - 与 TypeScript replace_tool 对齐
pub struct EditFileTool {
    pub cwd: String,
}

impl EditFileTool {
    pub fn new(cwd: impl Into<String>) -> Self {
        Self { cwd: cwd.into() }
    }
}

#[async_trait::async_trait]
impl BaseTool for EditFileTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn is_direct(&self) -> bool {
        true
    }

    /// 同类工具分组（design v2 §2.5.1）：filesystem 工具统一归组。
    fn namespace(&self) -> Option<&str> {
        Some("filesystem")
    }

    /// 提示词层声明模板（design v2 §2.5.3）：对应 05 段落 "Write or edit a file"
    /// 条目语义（选择指引 + 纪律约束），不逐字重复（守护测试断言）。
    /// title 不覆盖——走 `tool_description` 默认推导路径。
    fn prompt_declaration(&self) -> Option<String> {
        Some(
            "Edit a file → `{{name}}` (targeted diff). Use `{{name}}` for targeted edits, not `echo >`/`sed`/`awk`."
                .to_string(),
        )
    }

    fn description(&self) -> &str {
        EDIT_FILE_DESCRIPTION
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to modify"
                },
                "old_string": {
                    "type": "string",
                    "description": "The text to replace. Must match EXACTLY including all whitespace, indentation, and newlines. The edit will fail if old_string is not unique in the file unless replace_all is true"
                },
                "new_string": {
                    "type": "string",
                    "description": "The text to replace it with (must be different from old_string)"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "If true, replace all occurrences of old_string. If false (default), replace only the first occurrence. Use this to rename variables or update repeated patterns across the file"
                },
                "expected_sha256": {
                    "type": "string",
                    "description": "Optional optimistic-concurrency guard: the SHA-256 hex digest of the file content you last read. When provided and the current file no longer matches, the edit is rejected with a conflict instead of applying against newer changes."
                }
            },
            "required": ["file_path", "old_string", "new_string"]
        })
    }

    async fn invoke(
        &self,
        input: Value,
        _ctx: peri_agent::tools::ToolContext<'_>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let file_path = input["file_path"].as_str().ok_or_else(|| {
            ToolFailure::new(
                "The 'file_path' parameter is required for the Edit tool.",
                "The 'file_path' parameter is required for the Edit tool.",
            )
        })?;
        let old_string = input["old_string"].as_str().ok_or_else(|| {
            ToolFailure::new(
                "The 'old_string' parameter is required for the Edit tool.",
                "The 'old_string' parameter is required for the Edit tool.",
            )
        })?;
        let new_string = input["new_string"].as_str().ok_or_else(|| {
            ToolFailure::new(
                "The 'new_string' parameter is required for the Edit tool.",
                "The 'new_string' parameter is required for the Edit tool.",
            )
        })?;
        let replace_all = input["replace_all"].as_bool().unwrap_or(false);

        if old_string.is_empty() {
            return Err(ToolFailure::new(
                "old_string cannot be empty. Read the file and supply the exact text to replace.",
                "Error: old_string cannot be empty",
            )
            .into());
        }
        // 描述声明 "must be different from old_string"，实现必须兑现
        // （executor review 2026-10-26 P0-4：相同值不应走完整读改写并报成功）。
        if old_string == new_string {
            return Err(ToolFailure::new(
                "new_string is identical to old_string; an edit must change the text. Adjust new_string, or use Read to verify the file before retrying.",
                "Error: new_string is identical to old_string; an edit must change the text.",
            )
            .into());
        }
        let expected_sha256 = match parse_expected_sha256(&input["expected_sha256"]) {
            Ok(value) => value,
            Err(message) => {
                return Err(ToolFailure::new(
                    "expected_sha256 must be a 64-character hex SHA-256 digest.",
                    message,
                )
                .into())
            }
        };

        let resolved = resolve_path(&self.cwd, file_path);
        let cwd = self.cwd.clone();

        match apply_edit(
            &resolved,
            &cwd,
            old_string,
            new_string,
            replace_all,
            expected_sha256.as_deref(),
        ) {
            Ok(message) => Ok(message),
            Err(EditCoreError::FileNotFound) => Err(ToolFailure::new(
                super::path_hints::with_path_hint(
                    "File not found. Verify file_path or locate the file with Glob.",
                    &self.cwd,
                    &resolved,
                ),
                format!("Error: File not found at {}", resolved.display()),
            )
            .into()),
            Err(EditCoreError::ReadFailed) => Err(ToolFailure::new(
                "Edit failed while reading the file. Verify access permissions and UTF-8 encoding.",
                format!("Edit failed while reading {}", resolved.display()),
            )
            .into()),
            Err(EditCoreError::NotUtf8) => Err(ToolFailure::new(
                "Edit failed while reading the file. Verify access permissions and UTF-8 encoding.",
                format!("Edit failed while decoding {}", resolved.display()),
            )
            .into()),
            Err(EditCoreError::ShaMismatch) => {
                Err(ToolFailure::new(SHA_MISMATCH_REJECTION, SHA_MISMATCH_REJECTION).into())
            }
            Err(EditCoreError::OldNotFound { hint }) => Err(ToolFailure::new(
                "old_string not found. Read the file and copy the exact text, including whitespace.",
                format!(
                    "Error: old_string not found in {}\n{hint}",
                    resolved.display()
                ),
            )
            .into()),
            Err(EditCoreError::NotUnique {
                locations_text,
                occurrences,
            }) => Err(ToolFailure::new(
                "old_string is not unique. Provide more context to make it unique, or set replace_all=true.",
                format!(
                    "Error: old_string is not unique in {} (found {occurrences} occurrences).\nMatch locations: {locations_text}.\nPlease provide more context to make old_string unique, or set replace_all=true.",
                    resolved.display()
                ),
            )
            .into()),
            Err(EditCoreError::Sentinel) => {
                Err(ToolFailure::new(SENTINEL_REJECTION, SENTINEL_REJECTION).into())
            }
            Err(EditCoreError::CommitFailed) => Err(ToolFailure::new(
                "Edit failed while committing the file. Check write permissions and available disk space; Read before retrying.",
                format!("Edit failed while committing {}", resolved.display()),
            )
            .into()),
        }
    }
}

#[cfg(test)]
#[path = "edit_test.rs"]
mod tests;
