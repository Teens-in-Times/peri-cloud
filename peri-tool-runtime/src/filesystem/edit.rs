use peri_acp_types::tools::BaseTool;
use serde_json::Value;

use super::transaction::target_key;
#[allow(unused_imports)]
pub(crate) use super::transaction::SENTINEL_REJECTION;
use super::write::{parse_expected_sha256, SHA_MISMATCH_REJECTION};

/// Edit 核心层结构化错误：两侧封装各自映射成自己的用户文案。
#[derive(Debug)]
pub enum EditCoreError {
    FileNotFound,
    ReadFailed,
    NotUtf8,
    ShaMismatch,
    OldNotFound {
        hint: String,
    },
    NotUnique {
        locations_text: String,
        occurrences: usize,
    },
    Sentinel,
    CommitFailed,
}

/// Edit 应用核心（单一事实源，两侧封装共用）：锁内 读 → sha 护栏 → 定位/替换 →
/// 哨兵检查 → 原子提交，返回格式化好的成功消息。
pub fn apply_edit(
    resolved: &std::path::Path,
    cwd: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    expected_sha256: Option<&str>,
) -> Result<String, EditCoreError> {
    super::transaction::with_target_lock(resolved, |locked| {
        let pre = match locked.read_pre() {
            Ok(Some(content)) => content,
            Ok(None) => return Err(EditCoreError::FileNotFound),
            Err(_) => return Err(EditCoreError::ReadFailed),
        };
        if let Some(expected) = expected_sha256 {
            let actual = super::write::hex_sha256(&pre);
            if !actual.eq_ignore_ascii_case(expected) {
                return Err(EditCoreError::ShaMismatch);
            }
        }
        let content = match std::str::from_utf8(&pre) {
            Ok(content) => content,
            Err(_) => return Err(EditCoreError::NotUtf8),
        };

        let old_lines = old_string.lines().count();
        let new_lines = new_string.lines().count();
        let line_diff = new_lines as i64 - old_lines as i64;
        let rel = resolved
            .strip_prefix(cwd)
            .unwrap_or(resolved)
            .display()
            .to_string();

        // 构建行数变化描述。行数不变（Equal）时仍报告被替换的行数
        // （= old_string 行数，±0 也有变更量）——TUI 依赖摘要中的计数
        // 展示 `· +N · -N` 后缀；旧文本 "Replaced text (same line count)"
        // 无计数信息，TUI 无法展示变更摘要。
        let diff_desc = match line_diff.cmp(&0) {
            std::cmp::Ordering::Greater => format!(
                "Added {} line{}",
                line_diff,
                if line_diff == 1 { "" } else { "s" }
            ),
            std::cmp::Ordering::Less => format!(
                "Removed {} line{}",
                -line_diff,
                if -line_diff == 1 { "" } else { "s" }
            ),
            std::cmp::Ordering::Equal => format!(
                "Replaced {} line{}",
                old_lines,
                if old_lines == 1 { "" } else { "s" }
            ),
        };

        if replace_all {
            if !content.contains(old_string) {
                return Err(EditCoreError::OldNotFound {
                    hint: build_not_found_hint(content, old_string),
                });
            }
            let new_content = content.replace(old_string, new_string);
            let occurrences = content.matches(old_string).count();
            match locked.guard_and_commit(Some(pre), new_content.into_bytes()) {
                Ok(()) => Ok(format!(
                    "{} to {} (replaced {} occurrence{})",
                    diff_desc,
                    rel,
                    occurrences,
                    if occurrences == 1 { "" } else { "s" }
                )),
                Err(super::transaction::CommitError::Sentinel) => Err(EditCoreError::Sentinel),
                Err(super::transaction::CommitError::ShaMismatch { .. }) => {
                    Err(EditCoreError::ShaMismatch)
                }
                Err(super::transaction::CommitError::Io(_)) => Err(EditCoreError::CommitFailed),
            }
        } else {
            let occurrences = content.matches(old_string).count();
            if occurrences == 0 {
                return Err(EditCoreError::OldNotFound {
                    hint: build_not_found_hint(content, old_string),
                });
            }
            if occurrences > 1 {
                let locations: Vec<String> = content
                    .match_indices(old_string)
                    .take(10)
                    .map(|(offset, _)| {
                        let line = content[..offset].lines().count() + 1;
                        let end_line = line + old_string.lines().count().saturating_sub(1);
                        if end_line > line {
                            format!("lines {}-{}", line, end_line)
                        } else {
                            format!("line {}", line)
                        }
                    })
                    .collect();
                let locations_text = if occurrences > 10 {
                    format!(
                        "{} ({} total, showing first 10)",
                        locations.join(", "),
                        occurrences
                    )
                } else {
                    locations.join(", ")
                };
                return Err(EditCoreError::NotUnique {
                    locations_text,
                    occurrences,
                });
            }
            let new_content = content.replacen(old_string, new_string, 1);
            match locked.guard_and_commit(Some(pre), new_content.into_bytes()) {
                Ok(()) => Ok(format!("{} to {}", diff_desc, rel)),
                Err(super::transaction::CommitError::Sentinel) => Err(EditCoreError::Sentinel),
                Err(super::transaction::CommitError::ShaMismatch { .. }) => {
                    Err(EditCoreError::ShaMismatch)
                }
                Err(super::transaction::CommitError::Io(_)) => Err(EditCoreError::CommitFailed),
            }
        }
    })
}

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

/// 为 old_string not found 错误构建模糊匹配提示（单一事实源，两侧封装共用）。
///
/// 策略 1：取 old_string 前 5 行做前缀匹配，报告匹配到的行号范围。
/// 策略 2：前缀匹配失败时，用滑动窗口找最接近的区域，报告差异行数。
/// old_string > 5000 字符时跳过，仅返回建议 Read 提示。
pub fn build_not_found_hint(content: &str, old_string: &str) -> String {
    const MAX_FUZZY_LEN: usize = 5000;
    if old_string.len() > MAX_FUZZY_LEN {
        return "Please Read this file to get the latest content before retrying.".to_string();
    }

    // 策略 1：前缀匹配
    let prefix_lines: Vec<&str> = old_string.lines().take(5).collect();
    let prefix: String = prefix_lines.join("\n");
    if !prefix.is_empty() {
        if let Some(byte_offset) = content.find(&prefix) {
            let line_start = content[..byte_offset].lines().count() + 1;
            let line_end = line_start + prefix_lines.len() - 1;
            return format!(
                "old_string's first {} lines matched lines {}-{}, but the full string did not match. \
                 The file may have been modified. Please Read this file to get the latest content before retrying.",
                prefix_lines.len(),
                line_start,
                line_end
            );
        }
    }

    // 策略 2：行数近似匹配（回退）
    let old_lines: Vec<&str> = old_string.lines().collect();
    let file_lines: Vec<&str> = content.lines().collect();
    let window_len = old_lines.len();

    if window_len > 0 && window_len <= file_lines.len() {
        let mut best_pos = 0;
        let mut best_common = 0;

        for start in 0..=file_lines.len().saturating_sub(window_len) {
            let window = &file_lines[start..start + window_len];
            let common = window
                .iter()
                .zip(old_lines.iter())
                .filter(|(a, b)| a.trim() == b.trim())
                .count();
            if common > best_common {
                best_common = common;
                best_pos = start;
            }
        }

        if best_common > 0 {
            let line_start = best_pos + 1;
            let line_end = best_pos + window_len;
            let diff_count = window_len - best_common;
            return format!(
                "Closest match at lines {}-{} ({} of {} lines differ). \
                 Please Read this file to get the latest content before retrying.",
                line_start, line_end, diff_count, window_len
            );
        }
    }

    "Please Read this file to get the latest content before retrying.".to_string()
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
        _ctx: peri_acp_types::tools::ToolContext<'_>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let file_path = input["file_path"]
            .as_str()
            .ok_or("The 'file_path' parameter is required for the Edit tool.")?;
        let old_string = input["old_string"]
            .as_str()
            .ok_or("The 'old_string' parameter is required for the Edit tool.")?;
        let new_string = input["new_string"]
            .as_str()
            .ok_or("The 'new_string' parameter is required for the Edit tool.")?;
        let replace_all = input["replace_all"].as_bool().unwrap_or(false);

        if old_string.is_empty() {
            return Err("Error: old_string cannot be empty".into());
        }
        // 描述声明 "must be different from old_string"，实现必须兑现
        // （executor review 2026-10-26 P0-4：相同值不应走完整读改写并报成功）。
        if old_string == new_string {
            return Err(
                "Error: new_string is identical to old_string; an edit must change the text. \
                 Adjust new_string, or use Read to verify the file before retrying."
                    .into(),
            );
        }
        let expected_sha256 = match parse_expected_sha256(&input["expected_sha256"]) {
            Ok(value) => value,
            Err(message) => return Err(message.into()),
        };

        let resolved = target_key(&self.cwd, file_path);
        let cwd = self.cwd.clone();

        let result = apply_edit(
            &resolved,
            &cwd,
            old_string,
            new_string,
            replace_all,
            expected_sha256.as_deref(),
        );
        match result {
            Ok(message) => Ok(message),
            Err(EditCoreError::FileNotFound) => Err("Error: File not found".into()),
            Err(EditCoreError::ReadFailed) | Err(EditCoreError::NotUtf8) => {
                Err("Edit failed while reading the file.".into())
            }
            Err(EditCoreError::ShaMismatch) => Err(SHA_MISMATCH_REJECTION.into()),
            Err(EditCoreError::OldNotFound { hint }) => Err(format!(
                "Error: old_string not found in {}\n{hint}",
                resolved.display()
            )
            .into()),
            Err(EditCoreError::NotUnique {
                locations_text,
                occurrences,
            }) => Err(format!(
                "Error: old_string is not unique in {} (found {} occurrences).\nMatch locations: {locations_text}.\nPlease provide more context to make old_string unique, or set replace_all=true.",
                resolved.display(),
                occurrences
            )
            .into()),
            Err(EditCoreError::Sentinel) => {
                Err(super::transaction::SENTINEL_REJECTION.into())
            }
            Err(EditCoreError::CommitFailed) => {
                Err("Edit failed while committing the file.".into())
            }
        }
    }
}

#[cfg(test)]
#[path = "edit_test.rs"]
mod tests;
