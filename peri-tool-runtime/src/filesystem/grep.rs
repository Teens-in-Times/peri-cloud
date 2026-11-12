use std::{
    cell::Cell,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use grep::{
    regex::RegexMatcherBuilder,
    searcher::{BinaryDetection, SearcherBuilder},
};
use ignore::WalkBuilder;
use peri_acp_types::tools::BaseTool;
use serde_json::Value;
use tokio::time::{timeout, Duration};

use super::{
    grep_args::{GrepInput, OutputMode, ParsedArgs},
    grep_format::SearchSink,
};
use crate::budget::SEARCH_OUTPUT_BYTES;
use crate::output::persist_truncated_output;

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

/// 搜索超时：与 Glob 对齐（glob.rs `SCAN_TIMEOUT`），防止大目录树/慢正则长时间
/// 占用 blocking pool。超时后置位协作取消标志，walker 在检查点尽快退出
/// （spawn_blocking 无法强制 kill，协作停止是真实保护）。
pub const SEARCH_TIMEOUT: Duration = Duration::from_secs(15);

/// 输出字节预算：与 Glob 对齐（见 `crate::budget`）。
/// 超过后停止收集并把完整输出落盘（persist_truncated_output），内联只保留头部。
pub const MAX_OUTPUT_BYTES: usize = SEARCH_OUTPUT_BYTES;

/// 单行输出字节上限：minified/生成文件单行可达 MB 级，超出按 UTF-8 字符边界
/// 截断并追加可见标记（见 grep_format.rs `trim_line`）。
pub const MAX_LINE_BYTES: usize = 1_000;

/// walker 线程上限：`available_parallelism` 在 64 核机器上返回 64，不允许 spawn
/// 64 个 walker 线程——每个 worker 再跑独立 searcher，大仓库下 CPU 突发。
/// 8 线程足以并行 IO，上限避免线程数随核数线性放大。
pub const SEARCH_THREADS_MAX: usize = 8;

/// 结构化搜索错误：核心层不预设用户文案，由两侧封装各自映射
/// （远程执行器走朴素字符串，本地 workspace 实例附 ToolFailure/path 提示）。
#[derive(Debug)]
pub enum SearchError {
    /// 搜索路径不存在（携带解析后的路径供提示层使用）。
    PathNotFound(PathBuf),
    /// 正则非法（携带诊断详情）。
    InvalidPattern(String),
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SearchError::PathNotFound(path) => {
                write!(f, "Search path does not exist: {}", path.display())
            }
            SearchError::InvalidPattern(detail) => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for SearchError {}

/// 核心搜索函数（同步，在 spawn_blocking 中运行；单一事实源，两侧封装共用）。
///
/// `glob_filters`：invoke 已校验并编译的 `-g`/`type` 过滤器（空切片 = 不过滤）。
/// `cancelled`：invoke 超时置位。置位后 sink 立即停止当前文件搜索、walker 在
/// 检查点（walk 闭包开头、文件完成后）`Quit` 终止整个遍历。`stopped`（sink 与
/// walker 共享）由输出预算（行数/字节）置位，语义详见 grep_format.rs SearchSink
/// 文档——两者分离：超时无条件立即退出（含非 Default 模式），预算让非 Default
/// 模式数完当前文件再停（保证计数正确）。
///
/// `collection_limit` 是 offset 与 head_limit 合并后的收集上限（0 = 不限）：
/// 分页在结构化结果上完成，因此必须先收集到 offset+head_limit 才能定位窗口
/// （executor review 2026-10-26 P0-1：此前 offset 在 head_limit 截断后的字符串上
/// 应用，offset ≥ head_limit 的第二页永远为空）。
///
/// glob 过滤按「相对 search_path 的路径（/ 分隔）优先，basename 回退」匹配
/// （P0-3：此前只匹配 basename，`src/**/*.rs` 这类含路径的模式永不命中且无提示）。
///
pub fn execute_search(
    parsed: &ParsedArgs,
    glob_filters: &[glob::Pattern],
    cwd: &str,
    collection_limit: usize,
    head_limit: usize,
    offset: usize,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<String, SearchError> {
    // 构建搜索路径
    let search_path = match &parsed.path {
        Some(p) => {
            let p = Path::new(p);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                Path::new(cwd).join(p)
            }
        }
        None => PathBuf::from(cwd),
    };

    if !search_path.exists() {
        return Err(SearchError::PathNotFound(search_path));
    }

    // 构建 RegexMatcher
    let mut matcher_builder = RegexMatcherBuilder::new();
    matcher_builder
        .case_insensitive(parsed.case_insensitive)
        .word(parsed.whole_word);
    if parsed.multiline {
        matcher_builder.multi_line(true).dot_matches_new_line(true);
    }
    if parsed.fixed_strings {
        matcher_builder.fixed_strings(true);
    }
    let matcher = matcher_builder
        .build(&parsed.pattern)
        .map_err(|error| SearchError::InvalidPattern(error.to_string()))?;

    // 构建 WalkBuilder
    let mut builder = WalkBuilder::new(&search_path);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        // 线程 cap：`available_parallelism` 可能返回 64+，不允许 spawn 那么多
        // walker 线程（每个 worker 再跑独立 searcher）；8 线程足以并行 IO
        .threads(
            std::thread::available_parallelism()
                .map_or(1, |n| n.get())
                .min(SEARCH_THREADS_MAX),
        );
    if let Some(depth) = parsed.max_depth {
        builder.max_depth(Some(depth));
    }

    // `-g` glob 过滤器与 type 映射已在 invoke 中编译校验一次，
    // 这里只做词法匹配（语法错误在调用前即报错，不进入遍历）。
    let glob_filters = glob_filters.to_vec();

    // 共享状态
    // `results` 存 (排序 key=display_path, 输出行)：并行遍历下 push 顺序 = 线程调度
    // 顺序，输出前按 key 稳定排序保证确定性（同文件内 sink 行序稳定，排序保持）。
    let results = Arc::new(Mutex::new(Vec::new()));
    let total_lines = Arc::new(AtomicUsize::new(0));
    let total_bytes = Arc::new(AtomicUsize::new(0));
    let stopped = Arc::new(AtomicBool::new(false));
    let matcher = Arc::new(matcher);
    let cwd = Arc::new(cwd.to_string());
    let before_context = parsed.before_context;
    let after_context = parsed.after_context;

    // 并行搜索
    builder.build_parallel().run(|| {
        let matcher = Arc::clone(&matcher);
        let total_lines = Arc::clone(&total_lines);
        let total_bytes = Arc::clone(&total_bytes);
        let stopped = Arc::clone(&stopped);
        let cancel = cancel.clone();
        let cwd = Arc::clone(&cwd);
        let glob_filters = glob_filters.clone();
        let results = Arc::clone(&results);
        let search_path = search_path.clone();

        Box::new(
            move |entry_result: Result<ignore::DirEntry, ignore::Error>| {
                use ignore::WalkState;

                let entry = match entry_result {
                    Ok(e) => e,
                    Err(_) => return WalkState::Continue,
                };

                // 检查点：预算置位（stopped）、超时（cancelled）或宿主取消后协作退出整个遍历
                if stopped.load(Ordering::Relaxed) || cancel.is_cancelled() {
                    return WalkState::Quit;
                }
                if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                    return WalkState::Continue;
                }

                // -g glob 过滤：相对路径优先、basename 回退（P0-3）
                if !glob_filters.is_empty() {
                    let relative = entry
                        .path()
                        .strip_prefix(search_path.as_path())
                        .unwrap_or(entry.path())
                        .to_string_lossy()
                        .replace('\\', "/");
                    let file_name = entry.file_name().to_string_lossy();
                    let matched = glob_filters
                        .iter()
                        .any(|pattern| pattern.matches(&relative) || pattern.matches(&file_name));
                    if !matched {
                        return WalkState::Continue;
                    }
                }

                // 显示路径：相对于 cwd 的路径
                let display_path = entry
                    .path()
                    .strip_prefix(cwd.as_str())
                    .unwrap_or(entry.path())
                    .to_string_lossy()
                    .to_string();

                let mut searcher_builder = SearcherBuilder::new();
                searcher_builder
                    .line_number(parsed.line_number)
                    .binary_detection(BinaryDetection::quit(b'\x00'));
                if before_context > 0 {
                    searcher_builder.before_context(before_context);
                }
                if after_context > 0 {
                    searcher_builder.after_context(after_context);
                }
                if parsed.multiline {
                    searcher_builder.multi_line(true);
                }
                searcher_builder.invert_match(parsed.invert_match);
                let mut searcher = searcher_builder.build();

                let mut sink = SearchSink {
                    output_mode: parsed.output_mode,
                    local_lines: Vec::new(),
                    total_lines: Arc::clone(&total_lines),
                    total_bytes: Arc::clone(&total_bytes),
                    max_limit: collection_limit,
                    cancelled: cancel.clone(),
                    stopped: Arc::clone(&stopped),
                    display_path: display_path.clone(),
                    match_count: Cell::new(0),
                    has_match: Cell::new(false),
                    after_context,
                    before_context,
                    show_line_numbers: parsed.line_number,
                };

                match searcher.search_path(&*matcher, entry.path(), &mut sink) {
                    Ok(_) => {}
                    Err(_) => {
                        // 二进制文件等错误，跳过
                        return WalkState::Continue;
                    }
                }

                // Default 模式：本地缓冲批量入共享（一次锁，避免逐行全局锁竞争）
                if !sink.local_lines.is_empty() {
                    let mut r = results.lock().unwrap();
                    r.extend(
                        sink.local_lines
                            .drain(..)
                            .map(|line| (display_path.clone(), line)),
                    );
                }

                // FilesOnly / CountOnly / FilesWithoutMatch 模式在搜索完成后处理，
                // 每文件恰 1 行输出（收集上限语义 = 前 N 个文件）
                let file_line = if parsed.output_mode == OutputMode::FilesOnly
                    && sink.has_match.get()
                {
                    Some(display_path.clone())
                } else if parsed.output_mode == OutputMode::CountOnly && sink.match_count.get() > 0
                {
                    Some(format!("{}:{}", display_path, sink.match_count.get()))
                } else if parsed.output_mode == OutputMode::FilesWithoutMatch
                    && !sink.has_match.get()
                {
                    Some(display_path.clone())
                } else {
                    None
                };
                if let Some(line) = file_line {
                    let bytes = line.len();
                    let mut r = results.lock().unwrap();
                    r.push((display_path.clone(), line));
                    total_lines.fetch_add(1, Ordering::Relaxed);
                    if total_bytes.fetch_add(bytes, Ordering::Relaxed) + bytes > MAX_OUTPUT_BYTES {
                        stopped.store(true, Ordering::Relaxed);
                    }
                }

                // 文件级预算检查：输出行数达到收集上限则停新文件（所有模式统一）。
                // Default 模式 sink 预算保证单文件 ≤ collection_limit+1，此处负责跨文件
                // 精确停止；非 Default 模式 sink 忽略 stopped（数完当前文件），
                // 由这里停新文件。
                if collection_limit > 0 && total_lines.load(Ordering::Relaxed) >= collection_limit {
                    stopped.store(true, Ordering::Relaxed);
                }

                if stopped.load(Ordering::Relaxed) || cancel.is_cancelled() {
                    WalkState::Quit
                } else {
                    WalkState::Continue
                }
            },
        )
    });

    // 格式化输出
    let mut guard = results.lock().unwrap();
    // 稳定排序：跨文件按 display_path 字典序（并行遍历的 push 顺序 = 线程调度
    // 顺序，不排序则两次运行输出不同）；同文件内保持 sink 行序（行号有序）。
    guard.sort_by(|a, b| a.0.cmp(&b.0));
    if guard.is_empty() {
        return Ok("No matches found.".to_string());
    }
    // 分页在结构化结果上完成：先丢弃前 offset 条，再对窗口应用 head_limit。
    // 提示行/落盘提示只属于格式化阶段，绝不参与 offset 计数。
    let lines: Vec<String> = guard.drain(..).skip(offset).map(|(_, line)| line).collect();
    // 排序/取数后尽早释放锁，join/落盘在锁外进行
    drop(guard);

    if lines.is_empty() {
        return Ok(if stopped.load(Ordering::Relaxed) {
            format!(
                "No matches found after offset {offset} in the collected output (search stopped at the output limit)."
            )
        } else {
            format!("No matches found after offset {offset}.")
        });
    }

    let joined = lines.join("\n");
    let offset_note = if offset > 0 {
        format!(" after offset {offset}")
    } else {
        String::new()
    };
    let count_note = if stopped.load(Ordering::Relaxed) {
        format!(
            "{} output lines collected{offset_note}; total unknown",
            lines.len()
        )
    } else {
        format!("{} output lines total{offset_note}", lines.len())
    };

    // 行数精确截断：恰好 head_limit 行不标 truncated，超过才截断标记
    // （收集阶段预算 collection_limit+1，多收 1 行使"恰好 N"与"N+1"可区分）
    let mut output = joined.clone();
    let mut line_note = String::new();
    if head_limit > 0 && lines.len() > head_limit {
        let persist_hint = persist_truncated_output(&joined);
        output = lines[..head_limit].join("\n");
        line_note = format!("\n... (truncated at {} lines; {})", head_limit, count_note);
        line_note.push_str(&persist_hint);
    }

    // 字节预算兜底：行数截断后仍可能超限（head_limit=0 或超长行累积）。
    // 保留头部行（动态累计到字节预算），已收集输出落盘供 Read 查看。
    if output.len() > MAX_OUTPUT_BYTES {
        // 落盘全量 joined 而非行数截断版 output：行数/字节双截断触发时也不丢行
        // （落盘包含全部已收集行；早停后未扫描的匹配不在其中）
        let persist_hint = persist_truncated_output(&joined);
        let mut head: Vec<&str> = Vec::new();
        let mut head_bytes = 0usize;
        for line in output.split('\n') {
            if !head.is_empty() && head_bytes + line.len() + 1 > MAX_OUTPUT_BYTES {
                break;
            }
            head.push(line);
            head_bytes += line.len() + 1;
        }
        // 字节数是已收集 joined 的长度，不推断未扫描内容的总量。
        output = format!(
            "{}\n\n[Output truncated: {}, {} collected bytes; showing first {} — exceeds {} byte limit]{}",
            head.join("\n"),
            count_note,
            joined.len(),
            head.len(),
            MAX_OUTPUT_BYTES,
            persist_hint
        );
    } else if !line_note.is_empty() {
        output.push_str(&line_note);
    }

    Ok(output)
}

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
                    "description": "Number of context lines to show after and after each match (takes priority over -C). Alias of after_context"
                },
                "after_context": {
                    "type": "number",
                    "description": "Number of context lines to show after and after each match. Semantic alias of -A"
                },
                "-B": {
                    "type": "number",
                    "description": "Number of context lines to show before and after each match (takes priority over -C). Alias of before_context"
                },
                "before_context": {
                    "type": "number",
                    "description": "Number of context lines to show before each match (takes priority over -C). Semantic alias of -B"
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
        ctx: peri_acp_types::tools::ToolContext<'_>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let pattern = match input.get("pattern").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => return Err("Error: Missing required parameter 'pattern'".into()),
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
            context: crate::numeric::parse_optional_u64(
                input
                    .get("context")
                    .or_else(|| input.get("-C"))
                    .unwrap_or(&Value::Null),
                "context",
            )?
            .map(|n| n as usize),
            before_context: crate::numeric::parse_optional_u64(
                input
                    .get("before_context")
                    .or_else(|| input.get("-B"))
                    .unwrap_or(&Value::Null),
                "before_context",
            )?
            .map(|n| n as usize),
            after_context: crate::numeric::parse_optional_u64(
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
            head_limit: match crate::numeric::parse_optional_u64(
                &input["head_limit"],
                "head_limit",
            )? {
                Some(n) => n as usize,
                None => 250,
            },
            offset: crate::numeric::parse_optional_u64(&input["offset"], "offset")?
                .map(|n| n as usize),
            max_depth: crate::numeric::parse_optional_u64(&input["max_depth"], "max_depth")?
                .map(|n| n as usize),
        };

        let parsed = match grep_input.to_parsed_args() {
            Ok(p) => p,
            Err(e) => return Err(format!("Error: {e}").into()),
        };

        // glob 语法在遍历前校验一次：非法过滤器报错，不退化成「搜索全部文件」，
        // 也不在 walker 热循环里重复编译（`*.{ts,tsx}` 在这里展开成多个 pattern）。
        let glob_filters = match super::glob_patterns::compile_glob_filters(&parsed.glob_filters) {
            Ok(filters) => filters,
            Err(e) => {
                return Err(format!(
                    "Error: invalid glob pattern: {e}. Supported glob syntax is `*`, `?`, `[abc]` and `{{a,b}}` alternation (e.g. \"*.{{ts,tsx}}\", \"*.rs\")."
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
        let result = timeout(
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
            Err(_) => Err(format!(
                "Error: Search timed out after {} seconds. Please use a more specific pattern.",
                SEARCH_TIMEOUT.as_secs()
            )
            .into()),
            Ok(Err(e)) => Err(format!("Error: {e}").into()),
            Ok(Ok(Err(e))) => Err(format!("Error: {e}").into()),
            Ok(Ok(Ok(output))) => Ok(output),
        }
    }
}

#[cfg(test)]
#[path = "grep_test.rs"]
mod tests;

#[cfg(test)]
#[path = "grep_limits_test.rs"]
mod limits_tests;
