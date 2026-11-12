//! folder_operations 封装层（peri_agent::tools::BaseTool）。
//!
//! 列表/深扫核心（完整枚举、公平截断、symlink 标记、预算落盘）在
//! `peri-tool-runtime::filesystem::folder` 单一实现（P0-1b），
//! 本文件只做参数解析与 ToolFailure/path 提示映射。

use peri_mcp_common::failure::ToolFailure;

use peri_agent::tools::BaseTool;
use serde_json::Value;

use super::resolve_path;
pub use peri_tool_runtime::filesystem::folder::{deep_scan_folder, list_folder, MAX_LIST_ENTRIES};

/// folder_operations tool - 与 TypeScript folder_tool 对齐
pub struct FolderOperationsTool {
    pub cwd: String,
}

impl FolderOperationsTool {
    pub fn new(cwd: impl Into<String>) -> Self {
        Self { cwd: cwd.into() }
    }
}

const FOLDER_OPERATIONS_DESCRIPTION: &str = include_str!("descriptions/folder.md");

#[async_trait::async_trait]
impl BaseTool for FolderOperationsTool {
    fn name(&self) -> &str {
        "folder_operations"
    }

    fn is_direct(&self) -> bool {
        true
    }

    /// 同类工具分组（design v2 §2.5.1）：filesystem 工具统一归组。
    fn namespace(&self) -> Option<&str> {
        Some("filesystem")
    }

    /// 提示词层声明模板（design v2 §2.5.3）：对应 05 段落 "List directory contents"
    /// 条目语义（选择指引 + 纪律约束），不逐字重复（守护测试断言）。
    /// title 不覆盖——走 `tool_description` 默认推导路径。
    fn prompt_declaration(&self) -> Option<String> {
        Some(
            "List a directory / check structure → `{{name}}` (atomic, cross-platform, structured). Prefer `{{name}}` when entries are needed as data, `Bash ls -la` for quick one-shot human-readable output; avoid `mkdir`/`test -d` via `Bash`."
                .to_string(),
        )
    }

    fn description(&self) -> &str {
        FOLDER_OPERATIONS_DESCRIPTION
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["create", "list", "exists", "deep_scan"],
                    "description": "The folder operation to perform: \"create\" to create a directory, \"list\" to list directory contents, \"exists\" to check if a path exists, \"deep_scan\" to recursively scan directory tree with depth control"
                },
                "folder_path": {
                    "type": "string",
                    "description": "The absolute path to the folder for the operation"
                },
                "recursive": {
                    "type": "boolean",
                    "description": "For \"create\" operation: whether to create parent directories if needed (default true). Ignored for other operations"
                },
                "max_depth": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 10,
                    "description": "For \"deep_scan\" operation: maximum recursion depth (1 = current directory only, 2 = one level deep, default 3, max 10). Ignored for other operations"
                }
            },
            "required": ["operation", "folder_path"]
        })
    }

    async fn invoke(
        &self,
        input: Value,
        ctx: peri_agent::tools::ToolContext<'_>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        // 宿主取消（P2-8）：受理前已取消的调用直接拒绝，不再做文件系统工作。
        if ctx.cancellation.is_cancelled() {
            return Err(ToolFailure::new(
                "Invocation was cancelled before the folder operation started.",
                "Error: invocation was cancelled before the folder operation started.",
            )
            .into());
        }
        let operation = input["operation"].as_str().ok_or_else(|| {
            ToolFailure::new("Missing operation parameter", "Missing operation parameter")
        })?;
        let folder_path = input["folder_path"].as_str().ok_or_else(|| {
            ToolFailure::new(
                "Missing folder_path parameter",
                "Missing folder_path parameter",
            )
        })?;
        let recursive = input["recursive"].as_bool().unwrap_or(true);

        let resolved = resolve_path(&self.cwd, folder_path);

        match operation {
            "create" => {
                if recursive {
                    std::fs::create_dir_all(&resolved)?;
                } else {
                    std::fs::create_dir(&resolved).map_err(|error| {
                        if error.kind() == std::io::ErrorKind::NotFound {
                            Box::new(ToolFailure::new(
                                "Parent directory does not exist. Create it first or set recursive=true.",
                                error.to_string(),
                            )) as Box<dyn std::error::Error + Send + Sync>
                        } else {
                            Box::new(error) as Box<dyn std::error::Error + Send + Sync>
                        }
                    })?;
                }
                Ok(format!(
                    "\u{2713} Folder created successfully at: {}",
                    resolved.display()
                ))
            }

            "exists" => {
                if resolved.exists() {
                    let kind = if resolved.is_dir() {
                        "Directory"
                    } else {
                        "File"
                    };
                    Ok(format!(
                        "\u{2713} Folder exists at: {}\n  Type: {kind}",
                        resolved.display()
                    ))
                } else {
                    Ok(format!(
                        "\u{2717} Folder does not exist at: {}",
                        resolved.display()
                    ))
                }
            }

            "list" => {
                if !resolved.exists() {
                    return Err(ToolFailure::new(
                        super::path_hints::with_path_hint(
                            "Folder not found. Verify folder_path.",
                            &self.cwd,
                            &resolved,
                        ),
                        format!("Folder not found: {}", resolved.display()),
                    )
                    .into());
                }
                if !resolved.is_dir() {
                    return Err(ToolFailure::new(
                        "Path exists but is not a folder. Supply a directory path.",
                        format!("Path exists but is not a folder: {}", resolved.display()),
                    )
                    .into());
                }
                list_folder(&resolved, &ctx.cancellation)
            }

            "deep_scan" => {
                if !resolved.exists() {
                    return Err(ToolFailure::new(
                        super::path_hints::with_path_hint(
                            "Folder not found. Verify folder_path.",
                            &self.cwd,
                            &resolved,
                        ),
                        format!("Folder not found: {}", resolved.display()),
                    )
                    .into());
                }
                if !resolved.is_dir() {
                    return Err(ToolFailure::new(
                        "Path exists but is not a folder. Supply a directory path.",
                        format!("Path exists but is not a folder: {}", resolved.display()),
                    )
                    .into());
                }
                // 非法类型（浮点/字符串/负数）显式报错，不再静默回退默认值；
                // 合法整数越界按描述 clamp 到 [1, 10]
                let max_depth =
                    match peri_mcp_common::parse_optional_u64(&input["max_depth"], "max_depth")? {
                        Some(n) => (n as usize).clamp(1, 10),
                        None => 3,
                    };
                deep_scan_folder(&resolved, max_depth, &ctx.cancellation)
            }

            other => Err(ToolFailure::new(
                "Unknown operation. Use create, exists, list, or deep_scan.",
                format!("Unknown operation: {other}"),
            )
            .into()),
        }
    }
}

#[cfg(test)]
#[path = "folder_test.rs"]
mod tests;
