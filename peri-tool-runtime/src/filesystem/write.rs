use peri_acp_types::tools::BaseTool;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};

use super::draft::{draft_hint_en, DraftAccessError, DraftStore};
use super::transaction::{target_key, with_target_lock, CommitError, SENTINEL_REJECTION};

const WRITE_FILE_DESCRIPTION: &str = include_str!("descriptions/write.md");
const WRITE_IO_ERROR: &str = "Write failed while committing the file.";
const DRAFT_UNKNOWN: &str =
    "Draft is unknown or no longer available. Retry by providing content directly.";
const DRAFT_TARGET_MISMATCH: &str =
    "Draft belongs to a different file_path. Retry with the original file_path or content.";

/// 版本护栏不匹配的错误（用户可见文案与两侧封装一致）。
pub const SHA_MISMATCH_REJECTION: &str = "Write rejected: expected_sha256 does not match the current file content. The file changed since it was last read; Read it again and retry with the updated expected_sha256.";

/// 校验 expected_sha256 参数形态：64 位十六进制（大小写不敏感）。
pub fn parse_expected_sha256(value: &Value) -> Result<Option<String>, String> {
    let Some(text) = value.as_str() else {
        if value.is_null() {
            return Ok(None);
        }
        return Err(
            "Error: 'expected_sha256' must be a 64-character hex SHA-256 string".to_string(),
        );
    };
    let text = text.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("__omit__") {
        return Ok(None);
    }
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(
            "Error: 'expected_sha256' must be a 64-character hex SHA-256 string".to_string(),
        );
    }
    Ok(Some(text.to_ascii_lowercase()))
}

/// 单一事实源的提交核心：锁内 读 pre → 可选 sha 护栏 → 哨兵检查 → 原子提交。
///
/// 返回提交后的总行数（append 时 = 原内容 + 新内容）。
/// 本地 workspace 实例与远程执行器两侧的 Write 封装都委托到这里
/// （executor review 2026-10-26 P0-1b）。
pub fn commit_guarded(
    target: &std::path::Path,
    content: &str,
    append: bool,
    expected_sha256: Option<&str>,
) -> Result<usize, CommitError> {
    with_target_lock(target, |locked| {
        let pre = locked.read_pre().map_err(CommitError::Io)?;
        let existing = pre.as_deref().unwrap_or_default();
        if let Some(expected) = expected_sha256 {
            let actual = hex_sha256(existing);
            if pre.is_none() || !actual.eq_ignore_ascii_case(expected) {
                return Err(CommitError::ShaMismatch {
                    expected: expected.to_string(),
                    actual,
                });
            }
        }
        let post = if append {
            let mut post = Vec::with_capacity(existing.len() + content.len());
            post.extend_from_slice(existing);
            post.extend_from_slice(content.as_bytes());
            post
        } else {
            content.as_bytes().to_vec()
        };
        let total_lines = if post.is_empty() {
            0
        } else {
            post.split(|byte| *byte == b'\n').count() - usize::from(post.last() == Some(&b'\n'))
        };
        locked.guard_and_commit(pre, post)?;
        Ok(total_lines)
    })
}

pub fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// 提交成功后的回执文案（单一事实源）。
pub fn success_message(
    cwd: &str,
    target: &std::path::Path,
    content: &str,
    append: bool,
    total_lines: usize,
) -> String {
    let line_count = content.lines().count();
    let lines_label = if line_count == 1 { "line" } else { "lines" };
    let rel = target.strip_prefix(cwd).unwrap_or(target).display();
    if append {
        format!("Appended {line_count} {lines_label} to {rel} (file total: {total_lines} lines)")
    } else {
        format!("Wrote {line_count} {lines_label} {rel}")
    }
}

/// Write tool - 与 TypeScript write_tool 对齐
pub struct WriteFileTool {
    pub cwd: String,
    /// 失败草稿存储(进程级内存);None = PERI_WRITE_DRAFT=0 关闭
    drafts: Option<Arc<Mutex<DraftStore>>>,
}

impl WriteFileTool {
    pub fn new(cwd: impl Into<String>) -> Self {
        Self::with_draft(cwd, super::draft::draft_enabled())
    }

    /// 测试注入构造:enabled=false 时完全禁用草稿(不创建 store)
    pub fn with_draft(cwd: impl Into<String>, enabled: bool) -> Self {
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

    fn commit(
        &self,
        target: &std::path::Path,
        content: &str,
        append: bool,
        expected_sha256: Option<&str>,
    ) -> Result<usize, CommitError> {
        commit_guarded(target, content, append, expected_sha256)
    }

    fn direct_write(
        &self,
        target: &std::path::Path,
        target_id: &str,
        content: &str,
        append: bool,
        expected_sha256: Option<&str>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        match self.commit(target, content, append, expected_sha256) {
            Ok(total_lines) => Ok(success_message(
                &self.cwd,
                target,
                content,
                append,
                total_lines,
            )),
            Err(CommitError::Sentinel) => Err(SENTINEL_REJECTION.into()),
            Err(CommitError::ShaMismatch { .. }) => Err(SHA_MISMATCH_REJECTION.into()),
            Err(CommitError::Io(_)) => {
                let hint = self
                    .save_draft(target_id, content, append)
                    .map(|id| draft_hint_en(&id, content))
                    .unwrap_or_default();
                Err(format!("{WRITE_IO_ERROR}{hint}").into())
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
            return Err(DRAFT_UNKNOWN.into());
        };
        let mut store = store.lock().unwrap();
        match store.with_exact_entry(draft_id, target_id, |content, append| {
            self.commit(target, content, append, expected_sha256)
                .map(|total_lines| success_message(&self.cwd, target, content, append, total_lines))
        }) {
            Ok(message) => Ok(message),
            Err(DraftAccessError::Unknown) => Err(DRAFT_UNKNOWN.into()),
            Err(DraftAccessError::WrongTarget) => Err(DRAFT_TARGET_MISMATCH.into()),
            Err(DraftAccessError::Operation(CommitError::Sentinel)) => {
                Err(SENTINEL_REJECTION.into())
            }
            Err(DraftAccessError::Operation(CommitError::ShaMismatch { .. })) => {
                Err(SHA_MISMATCH_REJECTION.into())
            }
            Err(DraftAccessError::Operation(CommitError::Io(_))) => Err(WRITE_IO_ERROR.into()),
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
                    "description": "The full content to write to the file. Either 'content' or 'from_draft' must be provided. An explicitly provided empty string truncates the file to 0 bytes; omit the field (or pass \"__omit__\") to leave content unspecified."
                },
                "from_draft": {
                    "type": "string",
                    "description": "A draft id returned in a previous Write error message. Recover the failed write without resending content. Mutually exclusive with 'content'; reuse the original file_path."
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
        _ctx: peri_acp_types::tools::ToolContext<'_>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let file_path = input["file_path"]
            .as_str()
            .ok_or("The 'file_path' parameter is required for the Write tool.")?;
        let target = target_key(&self.cwd, file_path);
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
            Err(message) => return Err(message.into()),
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
        Err("Either 'content' or 'from_draft' must be provided for the Write tool.".into())
    }
}

#[cfg(test)]
#[path = "write_test.rs"]
mod tests;
