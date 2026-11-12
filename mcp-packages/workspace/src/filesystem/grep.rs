//! Grep 封装层（peri_agent::tools::BaseTool）。
//!
//! 搜索核心 `execute_search` 在 `peri-tool-runtime` 单一实现（P0-1b），
//! 本文件只做参数解析、glob 编译校验、错误文案映射（ToolFailure + path 提示）。

#[allow(unused_imports)]
use std::time::Duration;

use peri_agent::tools::BaseTool;
use peri_mcp_common::failure::ToolFailure;
use serde_json::Value;

// 核心层单一事实源；常量再导出给守护测试（grep_guard_test 断言取值）。
#[allow(unused_imports)]
pub(crate) use peri_tool_runtime::filesystem::grep::{
    execute_search, SearchError, MAX_LINE_BYTES, MAX_OUTPUT_BYTES, SEARCH_THREADS_MAX,
    SEARCH_TIMEOUT,
};
#[allow(unused_imports)]
pub(crate) use peri_tool_runtime::filesystem::grep_args::{GrepInput, OutputMode, ParsedArgs};

/// Grep tool - 与 Claude Code Grep 工具对齐
pub struct GrepTool {
    pub cwd: String,
}

impl GrepTool {
    pub fn new(cwd: impl Into<String>) -> Self {
        Self { cwd: cwd.into() }
    }
}

const GREP_DESCRIPTION: &str = include_str!("descriptions/grep.md");

#[async_trait::async_trait]
impl BaseTool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }

    fn is_direct(&self) -> bool {
        true
    }

    /// 同类工具分组（design v2 §2.5.1）：filesystem 工具统一归组。
    fn namespace(&self) -> Option<&str> {
        Some("filesystem")
    }

    /// 提示词层声明模板（design v2 §2.5.3）：对应 05 段落 "File content search"
    /// 条目语义（选择指引 + 纪律约束），不逐字重复（守护测试断言）。
    /// title 不覆盖——走 `tool_description` 默认推导路径。
    fn prompt_declaration(&self) -> Option<String> {
        Some(
            "Search file contents → `{{name}}` (regex, fast, scoped). Use `{{name}}` for content search, not `grep`/`rg`."
                .to_string(),
        )
    }

    fn description(&self) -> &str {
        GREP_DESCRIPTION
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The regular expression pattern to search for in file contents. Supports full regex syntax (e.g. \"log.*Error\", \"function\\s+\\w+\")"
                },
                "path": {
                    "type": "string",
                    "description": "File or directory path to search in. Defaults to current working directory if not specified"
                },
                "glob": {
                    "type": "string",
                    "description": "Glob pattern to filter files (e.g. \"*.js\", \"*.{ts,tsx}\", \"src/**/*.rs\"). A pattern matches when it matches the file's path relative to the search root ('/'-separated) or the file name alone. Only matching files will be searched"
                },
                "type": {
                    "type": "string",
                    "description": "Filter files by type. Common values: \"rust\", \"js\", \"py\", \"go\", \"java\", \"ts\". More efficient than glob for type-based filtering"
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count", "files_without_matches"],
                    "description": "Output mode: \"content\" shows matching lines with line numbers (default), \"files_with_matches\" lists only file paths, \"count\" shows match counts per file, \"files_without_matches\" lists file paths without matches"
                },
                "-i": {
                    "type": "boolean",
                    "description": "Enable case-insensitive search (default: false). Alias of case_insensitive"
                },
                "case_insensitive": {
                    "type": "boolean",
                    "description": "Enable case-insensitive search (default: false). Semantic alias of -i"
                },
                "-C": {
                    "type": "number",
                    "description": "Number of context lines to show before and after each match. Alias of context"
                },
                "context": {
                    "type": "number",
                    "description": "Number of context lines to show before and after each match. Semantic alias of -C"
                },
                "-A": {
                    "type": "number",
                    "description": "Number of context lines to show after each match (takes priority over -C). Alias of after_context"
                },
                "after_context": {
                    "type": "number",
                    "description": "Number of context lines to show after each match. Semantic alias of -A"
                },
                "-B": {
                    "type": "number",
                    "description": "Number of context lines to show before each match (takes priority over -C). Alias of before_context"
                },
                "before_context": {
                    "type": "number",
                    "description": "Number of context lines to show before each match. Semantic alias of -B"
                },
                "-n": {
                    "type": "boolean",
                    "description": "Show line numbers (default: true). Alias of show_line_numbers"
                },
                "show_line_numbers": {
                    "type": "boolean",
                    "description": "Show line numbers (default: true). Semantic alias of -n"
                },
                "multiline": {
                    "type": "boolean",
                    "description": "Enable multiline mode where ^/$ match line boundaries and . matches newlines (default: false)"
                },
                "whole_word": {
                    "type": "boolean",
                    "description": "Match whole words only (default: false)"
                },
                "invert_match": {
                    "type": "boolean",
                    "description": "Invert match: show lines that do NOT match the pattern, equivalent to grep -v (default: false)"
                },
                "fixed_strings": {
                    "type": "boolean",
                    "description": "Treat pattern as a literal string instead of regex, equivalent to grep -F (default: false)"
                },
                "max_depth": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Maximum directory depth to search. Limits how deep the search traverses into subdirectories"
                },
                "head_limit": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Limit output to first N matching lines (default 250). Pass 0 for unlimited. Use sparingly — large result sets waste context"
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Skip the first N matching lines, then apply head_limit to the remaining window (pagination)"
                }
            },
            "required": ["pattern"]
        })
    }

    fn timeout(&self) -> Option<std::time::Duration> {
        None
    }

    async fn invoke(
        &self,
        input: Value,
        ctx: peri_agent::tools::ToolContext<'_>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let pattern = match input.get("pattern").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => {
                return Err(ToolFailure::new(
                    "Missing required parameter pattern.",
                    "Error: Missing required parameter 'pattern'",
                )
                .into());
            }
        };

        let grep_input = GrepInput {
            pattern,
            path: input
                .get("path")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            glob: input
                .get("glob")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            type_filter: input
                .get("type")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            output_mode: input
                .get("output_mode")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            case_insensitive: input
                .get("case_insensitive")
                .or_else(|| input.get("-i"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            context: peri_mcp_common::parse_optional_u64(
                input
                    .get("context")
                    .or_else(|| input.get("-C"))
                    .unwrap_or(&Value::Null),
                "context",
            )?
            .map(|n| n as usize),
            before_context: peri_mcp_common::parse_optional_u64(
                input
                    .get("before_context")
                    .or_else(|| input.get("-B"))
                    .unwrap_or(&Value::Null),
                "before_context",
            )?
            .map(|n| n as usize),
            after_context: peri_mcp_common::parse_optional_u64(
                input
                    .get("after_context")
                    .or_else(|| input.get("-A"))
                    .unwrap_or(&Value::Null),
                "after_context",
            )?
            .map(|n| n as usize),
            line_number: input
                .get("show_line_numbers")
                .or_else(|| input.get("-n"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            multiline: input
                .get("multiline")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            whole_word: input
                .get("whole_word")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            invert_match: input
                .get("invert_match")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            fixed_strings: input
                .get("fixed_strings")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            head_limit: match peri_mcp_common::parse_optional_u64(
                &input["head_limit"],
                "head_limit",
            )? {
                Some(n) => n as usize,
                None => 250,
            },
            offset: peri_mcp_common::parse_optional_u64(&input["offset"], "offset")?
                .map(|n| n as usize),
            max_depth: peri_mcp_common::parse_optional_u64(&input["max_depth"], "max_depth")?
                .map(|n| n as usize),
        };

        let parsed = match grep_input.to_parsed_args() {
            Ok(p) => p,
            Err(e) => return Err(ToolFailure::new("Invalid output_mode. Use content, files_with_matches, count, or files_without_matches.", format!("Error: {e}")).into()),
        };

        // glob 语法在遍历前校验一次：非法过滤器报错，不退化成「搜索全部文件」，
        // 也不在 walker 热循环里重复编译（`*.{ts,tsx}` 在这里展开成多个 pattern）。
        let glob_filters = match super::glob_patterns::compile_glob_filters(&parsed.glob_filters) {
            Ok(filters) => filters,
            Err(e) => {
                return Err(ToolFailure::new(
                    "Invalid glob pattern. Close character classes and comma-separated alternation groups; supported glob syntax is `*`, `?`, `[abc]` and `{a,b}` alternation (e.g. \"*.{ts,tsx}\", \"*.rs\").",
                    format!("Error: {e}"),
                )
                .into());
            }
        };

        let head_limit = grep_input.head_limit;
        let offset = grep_input.offset.unwrap_or(0);
        // offset 与 head_limit 在结构化结果上分页：必须先收集到窗口末尾。
        // head_limit == 0 表示不限，收集也不设上限（仍受 MAX_OUTPUT_BYTES 约束）。
        let collection_limit = if head_limit == 0 {
            0
        } else {
            offset.saturating_add(head_limit)
        };

        let cwd = self.cwd.clone();
        // 协作取消标志：超时置位后，spawn_blocking 内的 walker 在检查点尽快退出
        // （与 glob.rs 超时分支对齐；spawn_blocking 无法强制 kill）
        let cancel = ctx.cancellation.child_token();
        let _cancel_on_drop = cancel.clone().drop_guard();
        let result = peri_time::timeout(
            SEARCH_TIMEOUT,
            tokio::task::spawn_blocking({
                move || {
                    execute_search(
                        &parsed,
                        &glob_filters,
                        &cwd,
                        collection_limit,
                        head_limit,
                        offset,
                        cancel,
                    )
                }
            }),
        )
        .await;

        if ctx.cancellation.is_cancelled() {
            return Err("Grep search cancelled".into());
        }
        match result {
            Err(_) => {
                Err(ToolFailure::new(
                    "Search timed out. Please use a more specific pattern.",
                    format!(
                    "Error: Search timed out after {} seconds. Please use a more specific pattern.",
                    SEARCH_TIMEOUT.as_secs()
                ),
                )
                .into())
            }
            Ok(Err(e)) => Err(format!("Error: {e}").into()),
            Ok(Ok(Err(SearchError::PathNotFound(path)))) => {
                Err(ToolFailure::new(
                    super::path_hints::with_path_hint(
                        "Search path does not exist. Verify path or search from the workspace directory.",
                        &self.cwd,
                        &path,
                    ),
                    format!("Search path does not exist: {}", path.display()),
                )
                .into())
            }
            Ok(Ok(Err(SearchError::InvalidPattern(detail)))) => {
                Err(ToolFailure::new(
                    "Invalid regex pattern. Check for unclosed brackets () [] {} and escape special characters (\\ . \\* \\+); set fixed_strings=true for literal matching.",
                    detail,
                )
                .into())
            }
            Ok(Ok(Ok(output))) => Ok(output),
        }
    }
}

#[cfg(test)]
#[path = "grep_test.rs"]
mod tests;

#[cfg(test)]
#[path = "search_reporting_test.rs"]
mod search_reporting_tests;
