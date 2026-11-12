//! 工具输出预算的单一事实源（executor review 2026-10-26 P2-3）。
//!
//! 之前各工具各自硬编码：Read 5,000B / Grep+Glob 20,000B / Bash 预览
//! 5,000+3,000B / Bash output_char_limit 10,000，数值分散且无统一政策说明。
//! 全部集中到这里；改预算只动本文件。
//!
//! 政策（docs/output-budgets.md 有完整说明）：
//! - 单行工具（Read/Grep/Glob/Folder）内联输出用字节预算，超限截断/落盘提示；
//! - Bash 回执用 stdout/stderr 双预览 + 字符上限，全文走日志文件；
//! - 落盘文件（persist_truncated_output）是字节预算的逃生通道，无字符预算。

/// Read 工具内联输出字节预算（超过按行截断并提示分段读取；Read 不落盘）。
pub const READ_OUTPUT_BYTES: usize = 5_000;

/// Grep / Glob / folder_operations 内联输出字节预算（超过落盘 + 头部内联）。
pub const SEARCH_OUTPUT_BYTES: usize = 20_000;

/// Bash 回执 stdout 预览字节预算。
pub const BASH_STDOUT_PREVIEW_BYTES: usize = 5_000;

/// Bash 回执 stderr 预览字节预算。
pub const BASH_STDERR_PREVIEW_BYTES: usize = 3_000;

/// Bash 工具的 output_char_limit（描述符声明给宿主的整体字符上限）。
pub const BASH_OUTPUT_CHAR_LIMIT: usize = 10_000;

/// 单条 Bash 日志流（stdout/stderr 落盘文件）的字节上限；到达后进程照常运行，
/// 落盘截断，回执标注 logs_truncated（executor review P1-1，参照 reach 16 MiB）。
pub const BASH_LOG_STREAM_LIMIT_BYTES: u64 = 16 * 1024 * 1024;
