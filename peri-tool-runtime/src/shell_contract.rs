//! Canonical Bash metadata and timeout semantics shared by both backends.

use serde_json::Value;

pub const BASH_DESCRIPTION: &str = include_str!("descriptions/bash.md");

/// 前台（同步）未传 timeout 时的默认超时：偏短，鼓励高效命令。
pub const FOREGROUND_DEFAULT_TIMEOUT_MS: u64 = 15_000;

/// 前台（同步）最大阻塞时长（硬上限）：显式 timeout 与 `timeout: 0` 都界到此值。
/// 同步执行恒有界——不存在禁用超时的路径；到达上限后进程不杀，
/// 而是 promote 为后台任务续跑（见 `BashTool::invoke_output`）。
pub const FOREGROUND_MAX_TIMEOUT_MS: u64 = 120_000;

/// 后台显式 timeout 的上限（后台不阻塞 Agent，允许更长的显式上限）。
pub const BACKGROUND_MAX_TIMEOUT_MS: u64 = 600_000;

/// 显式 timeout 的下限：Windows 进程创建/终止开销大，过短超时不可靠。
fn min_timeout_ms(windows: bool) -> u64 {
    if windows {
        5_000
    } else {
        1
    }
}

/// 解析前台（同步）timeout：结果恒为有界值。
///
/// 返回 `(生效毫秒, 请求值被改写时的原始值)`：
/// - 未传 → [`FOREGROUND_DEFAULT_TIMEOUT_MS`]，未改写
/// - 显式 `0` → [`FOREGROUND_MAX_TIMEOUT_MS`]（`0` 不能表示"不超时"），原始值为 `Some(0)`
/// - 显式 > [`FOREGROUND_MAX_TIMEOUT_MS`] → 上限值，原始值为请求值（供回执说明）
/// - 其余 → 请求值，并按平台下限兜底（Unix 1ms / Windows 5000ms）
pub fn parse_foreground_timeout(input: &serde_json::Value) -> (u64, Option<u64>) {
    parse_foreground_timeout_for_platform(input, cfg!(target_os = "windows"))
}

/// Remote callers use the bound device platform, rather than the cloud host OS.
pub fn parse_foreground_timeout_for_platform(
    input: &serde_json::Value,
    windows: bool,
) -> (u64, Option<u64>) {
    match input.get("timeout").and_then(|v| v.as_u64()) {
        None => (FOREGROUND_DEFAULT_TIMEOUT_MS, None),
        Some(0) => (FOREGROUND_MAX_TIMEOUT_MS, Some(0)),
        Some(ms) if ms > FOREGROUND_MAX_TIMEOUT_MS => (FOREGROUND_MAX_TIMEOUT_MS, Some(ms)),
        Some(ms) => (ms.max(min_timeout_ms(windows)), None),
    }
}

/// 解析后台 timeout：`None` = 不超时（后台语义：跑完为止，由 Tasks 面板取消）。
///
/// - 未传 / 显式 `0` → None
/// - 显式 >0 → clamp 到 [min, `BACKGROUND_MAX_TIMEOUT_MS`]
pub fn parse_background_timeout(input: &serde_json::Value) -> Option<u64> {
    match input.get("timeout").and_then(|v| v.as_u64()) {
        None | Some(0) => None,
        Some(ms) => Some(ms.clamp(
            min_timeout_ms(cfg!(target_os = "windows")),
            BACKGROUND_MAX_TIMEOUT_MS,
        )),
    }
}

#[cfg(test)]
#[path = "shell_contract_test.rs"]
mod tests;

pub fn bash_parameters() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "The bash command (and optional arguments) to execute. This can be complex commands that use pipes, &&, or other shell features. For multiple dependent commands, chain them with && rather than making separate calls"
            },
            "timeout": {
                "type": "number",
                "description": format!(
                    "Optional timeout in milliseconds. The synchronous path is always bounded: it defaults to 15000ms and is capped at {FOREGROUND_MAX_TIMEOUT_MS}ms (2 minutes) — `timeout: 0` is treated as that maximum instead of disabling the timeout, so no request can produce an unbounded synchronous wait. Foreground timeout returns an error: if background task registration succeeds, the process continues in the background with a task_id, pid and log file paths, without a new timeout; otherwise termination is requested. Background tasks (run_in_background: true) run until completion: omitting `timeout` or setting `0` leaves that background command without a timeout, and a positive `timeout` (up to 600000ms) requests termination when reached. Check the returned process status before retrying; do not duplicate a task that is still running. For builds, installs, or tests, set a longer `timeout` up to the foreground maximum, or use run_in_background: true for work that needs longer."
                )
            },
            "run_in_background": {
                "type": "boolean",
                "description": "If true, runs the command in the background and returns immediately with a task_id. Only use for long-running servers, watchers, or daemons. For builds/installs/tests, prefer a longer timeout instead."
            }
        },
        "required": ["command"]
    })
}
