//! Write 封装层（peri_agent::tools::BaseTool）。
//!
//! 提交核心（锁 + sha 护栏 + 哨兵检查 + fsync 原子提交）在
//! `peri-tool-runtime::filesystem::write::commit_guarded` 单一实现（P0-1b），
//! 本文件只做参数语义（空 content 合法、from_draft 恢复）与错误文案映射。

use peri_agent::tools::BaseTool;
use peri_mcp_common::failure::ToolFailure;
use serde_json::Value;
use std::sync::{Arc, Mutex};

use super::draft::{draft_hint_en, DraftAccessError, DraftStore};
use super::resolve_path;
#[allow(unused_imports)]
pub(crate) use super::transaction::SENTINEL_REJECTION;
use peri_tool_runtime::filesystem::transaction::CommitError;
use peri_tool_runtime::filesystem::write::{commit_guarded, success_message};
pub(crate) use peri_tool_runtime::filesystem::write::{
    parse_expected_sha256, SHA_MISMATCH_REJECTION,
};

const WRITE_FILE_DESCRIPTION: &str = include_str!("descriptions/write.md");
const WRITE_IO_ERROR: &str = "Write failed while committing the file.";
const DRAFT_UNKNOWN: &str =
    "Draft is unknown or no longer available (path/parameter errors never save drafts). Retry by providing content directly.";
const DRAFT_TARGET_MISMATCH: &str =
    "Draft belongs to a different file_path. Retry with the original file_path or content.";

/// Write tool - 与 TypeScript write_tool 对齐
pub struct WriteFileTool {
    pub cwd: String,
    /// 失败草稿存储（进程级内存）。
    drafts: Option<Arc<Mutex<DraftStore>>>,
}

impl WriteFileTool {
    pub fn new(cwd: impl Into<String>) -> Self {
        Self::with_draft(cwd, true)
    }

    /// 测试注入构造:enabled=false 时完全禁用草稿(不创建 store)
    pub(crate) fn with_draft(cwd: impl Into<String>, enabled: bool) -> Self {
        Self {
            cwd: cwd.into(),
            drafts: enabled.then(|| Arc::new(Mutex::new(DraftStore::new()))),
        }
    }

    fn save_draft(&self, target: &str, content: &str, append: bool) -> Option<String> {
        self.drafts.as_ref().map(|store| {
            store
                .lock()
                .unwrap()
                .save(target, content.to_string(), append)
        })
    }

    fn direct_write(
        &self,
        target: &std::path::Path,
        target_id: &str,
        content: &str,
        append: bool,
        expected_sha256: Option<&str>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        match commit_guarded(target, content, append, expected_sha256) {
            Ok(total_lines) => Ok(success_message(
                &self.cwd,
                target,
                content,
                append,
                total_lines,
            )),
            Err(CommitError::Sentinel) => Err(ToolFailure::new(
                super::transaction::SENTINEL_REJECTION,
                super::transaction::SENTINEL_REJECTION,
            )
            .into()),
            Err(CommitError::ShaMismatch { .. }) => {
                Err(ToolFailure::new(SHA_MISMATCH_REJECTION, SHA_MISMATCH_REJECTION).into())
            }
            Err(CommitError::Io(error)) => {
                let hint = self
                    .save_draft(target_id, content, append)
                    .map(|id| draft_hint_en(&id, content))
                    .unwrap_or_default();
                Err(ToolFailure::new(
                    format!("{WRITE_IO_ERROR}{hint}"),
                    format!(
                        "{WRITE_IO_ERROR}{hint}\nWrite failed at {}: {error}",
                        target.display()
                    ),
                )
                .into())
            }
        }
    }

    fn restore_write(
        &self,
        target: &std::path::Path,
        target_id: &str,
        draft_id: &str,
        expected_sha256: Option<&str>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let Some(store) = &self.drafts else {
            return Err(ToolFailure::new(DRAFT_UNKNOWN, DRAFT_UNKNOWN).into());
        };
        let mut store = store.lock().unwrap();
        match store.with_exact_entry(draft_id, target_id, |content, append| {
            commit_guarded(target, content, append, expected_sha256)
                .map(|total_lines| success_message(&self.cwd, target, content, append, total_lines))
        }) {
            Ok(message) => Ok(message),
            Err(DraftAccessError::Unknown) => {
                Err(ToolFailure::new(DRAFT_UNKNOWN, DRAFT_UNKNOWN).into())
            }
            Err(DraftAccessError::WrongTarget) => {
                Err(ToolFailure::new(DRAFT_TARGET_MISMATCH, DRAFT_TARGET_MISMATCH).into())
            }
            Err(DraftAccessError::Operation(CommitError::Sentinel)) => Err(ToolFailure::new(
                super::transaction::SENTINEL_REJECTION,
                super::transaction::SENTINEL_REJECTION,
            )
            .into()),
            Err(DraftAccessError::Operation(CommitError::ShaMismatch { .. })) => {
                Err(ToolFailure::new(SHA_MISMATCH_REJECTION, SHA_MISMATCH_REJECTION).into())
            }
            Err(DraftAccessError::Operation(CommitError::Io(error))) => Err(ToolFailure::new(
                WRITE_IO_ERROR,
                format!("Write failed at {}: {error}", target.display()),
            )
            .into()),
        }
    }
}

#[async_trait::async_trait]
impl BaseTool for WriteFileTool {
    fn name(&self) -> &str {
        "Write"
    }

    fn is_direct(&self) -> bool {
        true
    }

    fn namespace(&self) -> Option<&str> {
        Some("filesystem")
    }

    fn prompt_declaration(&self) -> Option<String> {
        Some(
            "Write a file → `{{name}}` (full contents). Use `{{name}}` for writing files, not `echo >`/`sed`/`awk`."
                .to_string(),
        )
    }

    fn description(&self) -> &str {
        WRITE_FILE_DESCRIPTION
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to write (must be absolute, not relative)"
                },
                "content": {
                    "type": "string",
                    "description": "The full content to write to the file. Required unless recovering a draft saved by a previous filesystem-stage write failure (see 'from_draft'). An explicitly provided empty string truncates the file to 0 bytes; omit the field (or pass \"__omit__\") to leave content unspecified."
                },
                "from_draft": {
                    "type": "string",
                    "description": "A 'draft_...' id from a previous Write error that failed while writing to disk. Only filesystem-stage failures save drafts; path/parameter validation errors never do (provide 'content' again in that case). Mutually exclusive with 'content'; reuse the original file_path."
                },
                "append": {
                    "type": "boolean",
                    "description": "If true, append content to the end of the file instead of overwriting. Use this for writing large files in chunks: first call Write without append to create the file with the initial content, then call Write with append=true to add more content. This avoids sending the entire file content in a single tool call, saving context window space.",
                    "default": false
                },
                "expected_sha256": {
                    "type": "string",
                    "description": "Optional optimistic-concurrency guard: the SHA-256 hex digest of the file content you last read. When provided and the current file no longer matches, the write is rejected with a conflict instead of silently overwriting newer changes. Compute it from the bytes you saw (an empty file hashes to the SHA-256 of an empty byte string)."
                }
            },
            "required": ["file_path"]
        })
    }

    fn timeout(&self) -> Option<std::time::Duration> {
        None
    }

    async fn invoke(
        &self,
        input: Value,
        _ctx: peri_agent::tools::ToolContext<'_>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let file_path = input["file_path"].as_str().ok_or_else(|| {
            ToolFailure::new(
                "The 'file_path' parameter is required for the Write tool.",
                "The 'file_path' parameter is required for the Write tool.",
            )
        })?;
        let target = resolve_path(&self.cwd, file_path);
        let target_id = target.to_string_lossy().to_string();
        // 字段缺失 ≠ 字段为空（executor review P0-2）：空串/纯空白是合法 content
        // （清空、截断、有意写空白），只有 "__omit__" 占位符与字段缺失走缺参路径。
        let content = input
            .get("content")
            .and_then(|value| value.as_str())
            .filter(|content| *content != "__omit__");
        let from_draft = input["from_draft"]
            .as_str()
            .filter(|id| !id.trim().is_empty() && *id != "__omit__");
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

        if let Some(content) = content {
            return self.direct_write(
                &target,
                &target_id,
                content,
                input["append"].as_bool().unwrap_or(false),
                expected_sha256.as_deref(),
            );
        }
        if let Some(draft_id) = from_draft {
            return self.restore_write(&target, &target_id, draft_id, expected_sha256.as_deref());
        }
        Err(ToolFailure::new(
            "Either content or from_draft must be provided for Write.",
            "Either 'content' or 'from_draft' must be provided for the Write tool.",
        )
        .into())
    }
}

#[cfg(test)]
#[path = "write_test.rs"]
mod tests;

#[cfg(test)]
#[path = "write_draft_test.rs"]
mod draft_tests;
