use peri_acp_types::tool_outcome::RawToolResult;
use peri_agent::tools::{BaseTool, ToolOutput};
use peri_mcp_common::failure::ToolFailure;
use serde_json::Value;

use super::resolve_path;
use peri_tool_runtime::filesystem::folder::list_folder;
#[allow(unused_imports)]
use peri_tool_runtime::filesystem::read::{
    is_binary_extension, render_lines, MAX_CHARS_PER_LINE, MAX_FILE_SIZE, MAX_LINES,
    MAX_OUTPUT_BYTES,
};

/// Read tool - 与 TypeScript read_tool 对齐
pub struct ReadFileTool {
    pub cwd: String,
}

impl ReadFileTool {
    pub fn new(cwd: impl Into<String>) -> Self {
        Self { cwd: cwd.into() }
    }

    /// 图片读取：请求路径或解析目标命中图片扩展名时返回带 image 内容块的 MCP 风格结果。
    ///
    /// `NotFound` / `NotFile` / `NotImage` 回落既有文本/二进制分支，让缺文件、
    /// 目录和伪装图片保持原有诊断；只有真正读到的图片才携带 base64 内容。
    async fn try_read_image(
        &self,
        input: &Value,
    ) -> Result<Option<ToolOutput>, Box<dyn std::error::Error + Send + Sync>> {
        let request = parse_read_request(input)?;
        let resolved = resolve_path(&self.cwd, request.file_path);
        // 同时看请求名与解析目标：`photo.png -> target` 的 symlink 不能因
        // canonicalize 丢失请求侧扩展名，反向链接也保持可用。
        let is_image = has_image_extension(std::path::Path::new(request.file_path))
            || has_image_extension(&resolved);
        if !is_image {
            return Ok(None);
        }
        let cwd = self.cwd.clone();
        let request_path = request.file_path.to_owned();
        let image = tokio::task::spawn_blocking(move || {
            crate::image::read_image_data(&cwd, &request_path, READ_IMAGE_MAX_BYTES)
        })
        .await
        .map_err(|_| {
            ToolFailure::new(
                "Image read failed. Retry the Read call.",
                "Error: Image read task failed",
            )
        })?;
        let image = match image {
            Ok(image) => image,
            Err(crate::image::ImageReadError::TooLarge) => {
                return Err(ToolFailure::new(
                    "Image too large. Resize the image or read it with a shell tool.",
                    format!(
                        "Error: Image too large (max {} bytes): {}",
                        READ_IMAGE_MAX_BYTES,
                        resolved.display()
                    ),
                )
                .into());
            }
            Err(crate::image::ImageReadError::CannotRead) => {
                return Err(ToolFailure::new(
                    "Cannot read image file. Verify permissions or use a shell tool.",
                    format!("Error: Cannot read image at {}", resolved.display()),
                )
                .into());
            }
            Err(crate::image::ImageReadError::NotFound) => {
                return Err(ToolFailure::new(
                    super::path_hints::with_path_hint(
                        "File not found. Verify file_path or locate the file with Glob.",
                        &self.cwd,
                        &resolved,
                    ),
                    format!("Error: File not found at {}", request.file_path),
                )
                .into());
            }
            Err(crate::image::ImageReadError::NotFile | crate::image::ImageReadError::NotImage) => {
                return Ok(None)
            }
        };
        let text = format!("Image read: {} ({}).", resolved.display(), image.media_type);
        let raw = RawToolResult::new(serde_json::json!({
            "content": [
                {"type": "text", "text": text.clone()},
                {"type": "image", "data": image.data, "mimeType": image.media_type},
            ]
        }));
        Ok(Some(ToolOutput::from_raw_result(text, raw)))
    }
}

/// Read 图片上限：10 MiB 解码后 base64 约 13.3 MiB，低于 transcript 单次
/// 持久化 16 MiB 预算；`@image` 附件通路仍按 `image::MAX_IMAGE_BYTES`。
const READ_IMAGE_MAX_BYTES: usize = 10 * 1024 * 1024;

const READ_FILE_DESCRIPTION: &str = include_str!("descriptions/read.md");

/// 解析 1-based 行号/行数参数（offset/limit）。
///
/// 语义与 schema 描述一致：offset 是 1-based 行号（1 = 首行），limit 是行数。
/// 缺省时返回 `default`；显式传入非正整数（0、负数、小数、非数字）一律报错，
/// 避免 `as_u64()` 对浮点静默回退为默认值、导致读到错误位置。
fn parse_line_number(
    value: &Value,
    name: &str,
    default: usize,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    if value.is_null() {
        return Ok(default);
    }
    let n = value.as_f64().ok_or_else(|| {
        ToolFailure::new(
            "offset and limit must be positive integers (1-based line numbers).",
            format!("Error: '{name}' must be a positive integer, got {value}"),
        )
    })?;
    if n.fract() != 0.0 || n < 1.0 {
        return Err(ToolFailure::new(
            "offset and limit must be positive integers (1-based line numbers).",
            format!("Error: '{name}' must be a positive integer (1-based line number), got {n}"),
        )
        .into());
    }
    Ok(n as usize)
}

/// 图片分支与文本分支共用的参数集合；图片不得绕过 pages/offset/limit 校验。
struct ReadRequest<'a> {
    file_path: &'a str,
    offset: usize,
    limit: usize,
}

fn parse_read_request(
    input: &Value,
) -> Result<ReadRequest<'_>, Box<dyn std::error::Error + Send + Sync>> {
    let file_path = input["file_path"]
        .as_str()
        .ok_or_else(|| ToolFailure::new("The 'file_path' parameter is required for the Read tool. Provide the absolute path to the file.", "The 'file_path' parameter is required for the Read tool. Provide the absolute path to the file."))?;

    // offset 语义为 1-based 行号（1 = 首行），与 schema 描述、输出行号一致；
    // 缺省 offset=1（读全文起点）、limit=2000。
    let offset = parse_line_number(&input["offset"], "offset", 1)?;
    let limit = parse_line_number(&input["limit"], "limit", MAX_LINES)?;

    if input.get("pages").is_some_and(|pages| !pages.is_null()) {
        let workaround = peri_acp_types::builtin_mcp::effective_name_of("workspace", "Bash")
            .map_or_else(
                || "Use a shell command with a PDF reader".to_string(),
                |name| format!("Use the `{name}` tool with a PDF reader command"),
            );
        let message = format!("The 'pages' parameter is not supported by Read. {workaround}.");
        return Err(ToolFailure::new(message.clone(), message).into());
    }

    Ok(ReadRequest {
        file_path,
        offset,
        limit,
    })
}

fn has_image_extension(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| is_image_extension(&ext.to_lowercase()))
}

fn is_image_extension(ext: &str) -> bool {
    matches!(ext, "png" | "jpg" | "jpeg" | "gif" | "webp")
}

#[async_trait::async_trait]
impl BaseTool for ReadFileTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn is_direct(&self) -> bool {
        true
    }

    /// 演示分组（design v2 §2.5.1 示例：同类工具按 namespace 组织声明段）。
    fn namespace(&self) -> Option<&str> {
        Some("filesystem")
    }

    /// 提示词层声明模板（design v2 §2.5.3 示例语义）。
    ///
    /// title 不覆盖——走 `BaseTool::tool_description` 默认路径由 name 推导
    /// （"Read" → "Read"），验证缺省推导在真实工具上生效。
    /// 全量迁移完成：声明段是工具选择指引的单一事实源，05 段落无对应条目
    /// （守护测试断言渲染输出与 05 剩余内容无逐字重复）。
    fn prompt_declaration(&self) -> Option<String> {
        Some(
            "Read a file → `{{name}}` ({{title}}). Use `{{name}}` for file content, not `cat`/`head`/`tail`."
                .to_string(),
        )
    }

    fn description(&self) -> &str {
        READ_FILE_DESCRIPTION
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to read"
                },
                "offset": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional 1-based start line. OMIT by default. NEVER guess or estimate this value, and never use a large offset to probe the end of a file. Set it only to a line number already observed in Read/Grep output or explicitly provided by the user. For continuation, use the last line actually shown plus 1; do not derive it from limit or an assumed file length"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "The maximum number of lines to read. Defaults to 2000 when omitted; use the continuation offset shown in the output to read further."
                }
            },
            "required": ["file_path"]
        })
    }

    fn aliases(&self) -> &[&str] {
        &["reading"]
    }

    async fn invoke_output(
        &self,
        input: Value,
        ctx: peri_agent::tools::ToolContext<'_>,
    ) -> Result<ToolOutput, Box<dyn std::error::Error + Send + Sync>> {
        if ctx.cancellation.is_cancelled() {
            return self.invoke(input, ctx).await.map(ToolOutput::from_legacy);
        }
        if let Some(output) = self.try_read_image(&input).await? {
            if ctx.cancellation.is_cancelled() {
                return Err(ToolFailure::new(
                    "Invocation was cancelled while reading the image.",
                    "Error: invocation was cancelled while reading the image.",
                )
                .into());
            }
            return Ok(output);
        }
        self.invoke(input, ctx).await.map(ToolOutput::from_legacy)
    }

    async fn invoke(
        &self,
        input: Value,
        ctx: peri_agent::tools::ToolContext<'_>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        // 宿主取消（P2-8）：受理前已取消的调用直接拒绝，不再读文件。
        if ctx.cancellation.is_cancelled() {
            return Err(ToolFailure::new(
                "Invocation was cancelled before the file was read.",
                "Error: invocation was cancelled before the file was read.",
            )
            .into());
        }
        let request = parse_read_request(&input)?;
        let offset = request.offset;
        let limit = request.limit;
        let file_path = request.file_path;
        let resolved = resolve_path(&self.cwd, file_path);

        if let Some(ext) = resolved.extension().and_then(|e| e.to_str()) {
            if is_binary_extension(&ext.to_lowercase()) {
                return Ok(format!(
                    "[BINARY FILE DETECTED]\n\nFile type: .{ext}\nFile path: {}\n\nThis is a binary file and cannot be displayed as text.",
                    resolved.display()
                ));
            }
        }

        let content = match std::fs::metadata(&resolved) {
            Ok(meta) if meta.len() > MAX_FILE_SIZE => {
                return Err(ToolFailure::new("File too large. offset/limit cannot bypass the file-size limit; use Grep or another file-processing tool.", format!(
                    "Error: File too large ({} bytes, max {} bytes). offset/limit cannot bypass the file-size limit; use Grep to locate content or another suitable file-processing tool.",
                    meta.len(),
                    MAX_FILE_SIZE
                )).into());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ToolFailure::new(
                    super::path_hints::with_path_hint(
                        "File not found. Verify file_path or locate the file with Glob.",
                        &self.cwd,
                        &resolved,
                    ),
                    format!("Error: File not found at {file_path}"),
                )
                .into());
            }
            Err(e) => return Err(e.into()),
            Ok(meta) if meta.is_dir() => {
                return list_folder(&resolved, &ctx.cancellation).map(|listing| {
                    format!(
                        "[DIRECTORY DETECTED]\n\nRead received a directory path and converted it to a directory listing. Use folder_operations with operation=\"list\" for explicit directory operations.\n\n{}",
                        listing
                    )
                });
            }
            Ok(meta) if meta.len() == 0 => {
                return Ok(format!(
                    "[EMPTY FILE]\n\nFile path: {}\nThe file is empty (0 bytes).",
                    resolved.display()
                ));
            }
            _ => match std::fs::read_to_string(&resolved) {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(ToolFailure::new(
                        super::path_hints::with_path_hint(
                            "File not found. Verify file_path or locate the file with Glob.",
                            &self.cwd,
                            &resolved,
                        ),
                        format!("Error: File not found at {file_path}"),
                    )
                    .into());
                }
                Err(e) => return Err(e.into()),
            },
        };

        // 大文件读取后再查一次取消（P2-8）：云端已取消的调用不做渲染。
        if ctx.cancellation.is_cancelled() {
            return Err(ToolFailure::new(
                "Invocation was cancelled before the file was rendered.",
                "Error: invocation was cancelled before the file was rendered.",
            )
            .into());
        }

        // 行号化 + 行内截断 + 输出预算在共享核心单一实现（P0-1b）。
        let output = render_lines(&content, offset, limit).map_err(|beyond| {
            ToolFailure::new(
                "offset exceeds file length. Omit offset to read from the beginning.",
                format!(
                    "Error: offset {} exceeds file length ({} lines). Valid offsets are 1..={}; omit offset to read from the beginning. Do not guess another offset or use offset to probe the file end.",
                    beyond.offset, beyond.total_lines, beyond.total_lines
                ),
            )
        })?;
        Ok(output)
    }

    fn prefers_persist(&self) -> bool {
        true
    }
}

#[cfg(test)]
#[path = "read_test.rs"]
mod tests;
