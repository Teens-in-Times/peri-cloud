//! Glob 封装层（peri_agent::tools::BaseTool）。
//!
//! 扫描核心（walk 规划 / root 收窄 / 收集 / 排序 / 预算截断）在
//! `peri-tool-runtime::filesystem::glob` 单一实现（P0-1b），
//! 本文件只做 pattern 编译校验、错误文案映射（ToolFailure + path 提示）
//! 与超时/取消编排。

use peri_mcp_common::failure::ToolFailure;
use std::path::Path;

use peri_agent::tools::BaseTool;
use peri_time::timeout;
use serde_json::Value;

use super::resolve_path;
#[allow(unused_imports)]
use super::should_skip_dir;
#[allow(unused_imports)]
pub(crate) use glob::Pattern;
#[cfg(test)]
use peri_tool_runtime::filesystem::glob::narrow_root;
use peri_tool_runtime::filesystem::glob::{plan_walk, run_glob, soft_warn_pattern, SCAN_TIMEOUT};

/// Glob tool — aligned with the TypeScript glob_tool.
pub struct GlobFilesTool {
    pub cwd: String,
}

impl GlobFilesTool {
    pub fn new(cwd: impl Into<String>) -> Self {
        Self { cwd: cwd.into() }
    }
}

const GLOB_FILES_DESCRIPTION: &str = include_str!("descriptions/glob.md");

#[async_trait::async_trait]
impl BaseTool for GlobFilesTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn is_direct(&self) -> bool {
        true
    }

    /// 同类工具分组（design v2 §2.5.1）：filesystem 工具统一归组。
    fn namespace(&self) -> Option<&str> {
        Some("filesystem")
    }

    /// 提示词层声明模板（design v2 §2.5.3）：对应 05 段落 "File name search"
    /// 条目语义（选择指引 + 纪律约束），不逐字重复（守护测试断言）。
    /// title 不覆盖——走 `tool_description` 默认推导路径。
    fn prompt_declaration(&self) -> Option<String> {
        Some(
            "Find files by name → `{{name}}` (e.g. `**/*.rs`, `*.config.json`). Use `{{name}}` for name search, not `Bash` with `find`; never `{{name}}(\"*\")`/`{{name}}(\"**/*\")` — that dumps the whole tree — list directories via `folder_operations` or `Bash ls`."
                .to_string(),
        )
    }

    fn description(&self) -> &str {
        GLOB_FILES_DESCRIPTION
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The glob pattern to match file paths relative to the search root (e.g. \"src/**/*.rs\", \"*.config.json\"). Wildcards follow the glob crate defaults: `*` and `?` match across `/`, so `*.rs` matches .rs files at ANY depth, not just the current directory. To scope the walk, use a literal directory prefix (e.g. \"src/*.rs\") or the path parameter"
                },
                "path": {
                    "type": "string",
                    "description": "The directory to search in. Absolute path or relative to cwd. If not specified, the current working directory is used"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn invoke(
        &self,
        input: Value,
        ctx: peri_agent::tools::ToolContext<'_>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let pattern = input["pattern"].as_str().ok_or_else(|| {
            ToolFailure::new(
                "The 'pattern' parameter is required for the Glob tool.",
                "The 'pattern' parameter is required for the Glob tool.",
            )
        })?;

        // Pattern 语法预校验 + 只编译一次：collect_files 复用编译结果，
        // 不再在逐文件热循环里重复 glob::Pattern::new。`{foo,bar}` 交替在这里展开
        // 成多个 pattern（glob crate 本身只按字面匹配 `{`）。
        let compiled = super::glob_patterns::compile_glob_patterns(pattern).map_err(|e| {
            ToolFailure::new(
                "Pattern syntax error. Use a valid glob pattern, e.g. *.rs (current dir), **/*.rs (recursive), src/**/*.rs, or {foo,bar}.rs (enumeration); brackets like [abc] must be closed.",
                format!("Error: Pattern syntax error in {pattern:?}: {e}"),
            )
        })?;

        // Pattern soft-warn — record the hint; we still execute so the LLM can see the output size and self-correct.
        let pattern_warn = soft_warn_pattern(pattern);

        let search_root = if let Some(p) = input["path"].as_str() {
            resolve_path(&self.cwd, p)
        } else {
            Path::new(&self.cwd).to_path_buf()
        };

        if !search_root.exists() {
            return Err(ToolFailure::new(
                super::path_hints::with_path_hint(
                    "Directory not found. Verify the search path.",
                    &self.cwd,
                    &search_root,
                ),
                format!("Error: Directory not found: {}", search_root.display()),
            )
            .into());
        }

        let plan = plan_walk(pattern);
        let cancel = ctx.cancellation.child_token();
        let _cancel_on_drop = cancel.clone().drop_guard();

        // 扫描是同步阻塞链（遍历 + 排序 + 落盘），必须移入 blocking pool，不能占用
        // async runtime worker。取消后，线程在下一个检查点协作退出
        // （spawn_blocking 无法强制 kill，协作停止是真实保护）。
        let scan = tokio::task::spawn_blocking({
            move || run_glob(&search_root, &compiled, &plan, &cancel)
        });

        let body = match timeout(SCAN_TIMEOUT, scan).await {
            Err(_) => {
                // 注意：超时与协作取消路径无法在确定性测试中覆盖（SCAN_TIMEOUT 为 15s，
                // 测试中不可触发），该路径仅由代码审查保障，未经自动化验证。
                return Err(ToolFailure::new(
                    "Search timed out. Please use a more specific pattern.",
                    "Error: Search timed out after 15 seconds. Please use a more specific pattern.",
                )
                .into());
            }
            Ok(Err(e)) => return Err(format!("Error: {e}").into()),
            Ok(Ok(body)) => body,
        };

        if ctx.cancellation.is_cancelled() {
            return Err("Glob scan cancelled".into());
        }
        if let Some(warn) = pattern_warn {
            Ok(format!("Note: {warn}\n\n{body}"))
        } else {
            Ok(body)
        }
    }
}

#[cfg(test)]
#[path = "glob_test.rs"]
mod tests;
