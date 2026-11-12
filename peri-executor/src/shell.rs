use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use peri_acp_types::tools::{ToolExecutionEvidence, ToolExecutionStatus, ToolOutput};
use peri_process::ProcessTree;
use peri_tool_runtime::budget::{
    BASH_LOG_STREAM_LIMIT_BYTES, BASH_STDERR_PREVIEW_BYTES, BASH_STDOUT_PREVIEW_BYTES,
};
use peri_tool_runtime::output::truncate_bytes;
use peri_tool_runtime::shell::shell_command;
use peri_tool_runtime::shell_contract::parse_background_timeout;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::store::Store;
use crate::{Error, Result};
use peri_acp_types::device_executor::{JobFailure, JobSnapshot, JobStatus, SubmitJob};

pub(crate) struct Outcome {
    pub status: JobStatus,
    pub output: Option<ToolOutput>,
    pub failure: Option<JobFailure>,
}

enum Stop {
    Natural(std::io::Result<ExitStatus>),
    Cancelled,
    Deadline,
}

/// 执行器侧总运行硬上限（P1-2）：前台命令在云端等待超时后按契约继续在设备上
/// 运行（promote-to-background 语义），但没有任何机制终止它的时代结束了——
/// 到达硬上限即 TerminateJobObject 并落 TimedOut。默认 24h，
/// 可用 `PERI_EXECUTOR_JOB_HARD_CAP_MS` 覆盖（clamp 到 [1min, 30d]）。
fn job_hard_cap() -> Duration {
    let default_ms: u64 = 24 * 60 * 60 * 1000;
    let configured = std::env::var("PERI_EXECUTOR_JOB_HARD_CAP_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(default_ms);
    Duration::from_millis(configured.clamp(60_000, 30 * 24 * 60 * 60 * 1000))
}

/// 单流日志排水任务：写入直到 [`BASH_LOG_STREAM_LIMIT_BYTES`]，超限后继续读并
/// 丢弃（进程照常跑，不会被反压卡死），文件尾追加截断标记（P1-1）。
async fn drain_bounded(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    mut file: tokio::fs::File,
    limit: u64,
) -> std::io::Result<bool> {
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};

    let mut written: u64 = 0;
    let mut truncated = false;
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if truncated {
            continue; // 丢弃，继续排空管道
        }
        let room = limit.saturating_sub(written) as usize;
        let take = read.min(room);
        if take > 0 {
            file.write_all(&buffer[..take]).await?;
            written += take as u64;
        }
        if take < read {
            let marker = format!("\n[log truncated at {limit} bytes]");
            let marker = &marker.as_bytes()[..marker.len().min(limit as usize)];
            let end = limit - marker.len() as u64;
            file.seek(std::io::SeekFrom::Start(end)).await?;
            file.write_all(marker).await?;
            file.flush().await?;
            let _ = file.sync_all().await;
            truncated = true;
        }
    }
    if !truncated {
        file.flush().await?;
        let _ = file.sync_all().await;
    }
    Ok(truncated)
}

pub(crate) async fn run(
    request: &SubmitJob,
    workspace: &str,
    root: &Path,
    store: &Store,
    cancellation: CancellationToken,
) -> Result<Outcome> {
    let dir = root.join("logs").join(request.invocation_id.to_string());
    tokio::fs::create_dir_all(&dir).await?;
    let stdout_path = dir.join("stdout.log");
    let stderr_path = dir.join("stderr.log");
    // Both paths are readable before the running receipt exposes them.
    let stdout_file = tokio::fs::File::create(&stdout_path).await?;
    let stderr_file = tokio::fs::File::create(&stderr_path).await?;
    // P1-1：不再把文件句柄直接交给子进程——经管道排水，超限落盘截断 + 丢弃续读。
    let command = request.input["command"]
        .as_str()
        .ok_or_else(|| Error::Invalid("Bash requires command".into()))?;
    // Windows 编码诚实性：PowerShell 对重定向句柄默认用 OEM 代码页输出，
    // 非 ASCII 文本会被替换成 '?'（丢信息且不可逆）。执行器侧统一把
    // [Console]::OutputEncoding 提到 UTF-8，剩余的无效字节仍由预览的
    // encoding_loss 标注兜底。
    #[cfg(windows)]
    let command = format!("[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; {command}");
    #[cfg(windows)]
    let command = command.as_str();
    let mut shell = shell_command(command, &[]);
    shell
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut tree = ProcessTree::new()?;
    tree.prepare(&mut shell);
    let mut child = match shell.spawn() {
        Ok(child) => child,
        Err(_) => {
            return Ok(failure_outcome(
                JobStatus::Failed,
                "shell_start_failed",
                "Could not start the platform shell; no process was launched.",
            ))
        }
    };
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_drain = stdout_pipe.map(|pipe| {
        tokio::spawn(drain_bounded(
            pipe,
            stdout_file,
            BASH_LOG_STREAM_LIMIT_BYTES,
        ))
    });
    let stderr_drain = stderr_pipe.map(|pipe| {
        tokio::spawn(drain_bounded(
            pipe,
            stderr_file,
            BASH_LOG_STREAM_LIMIT_BYTES,
        ))
    });
    if tree.attach(&child).is_err() {
        tree.terminate();
        let _ = child.start_kill();
        let confirmed = tokio::time::timeout(Duration::from_secs(10), async {
            child.wait().await?;
            tree.wait_for_exit().await;
            Ok::<_, std::io::Error>(())
        })
        .await
        .is_ok_and(|r| r.is_ok());
        return Ok(failure_outcome(
            if confirmed {
                JobStatus::Failed
            } else {
                JobStatus::RecoveryRequired
            },
            "process_ownership_failed",
            "Could not establish process-tree ownership; cleanup was requested.",
        ));
    }
    // If recording process evidence fails, this function retains ownership and
    // settles cleanup before returning. Never leave a launched, unowned child.
    if let Err(error) = store
        .update(request.invocation_id, |job| {
            job.pid = child.id();
            job.stdout_path = Some(stdout_path.to_string_lossy().into_owned());
            job.stderr_path = Some(stderr_path.to_string_lossy().into_owned());
        })
        .await
    {
        tree.terminate();
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(10), async {
            let _ = child.wait().await;
            tree.wait_for_exit().await;
        })
        .await;
        return Err(error);
    }
    let background = request.input["run_in_background"]
        .as_bool()
        .unwrap_or(false);
    // P1-2：前台命令此前在执行器侧无任何期限（云端等待超时后无人终止它）。
    // 显式后台 timeout 仍然生效；所有 job（前台含 promote-to-background 续跑）
    // 都套上执行器硬上限，到期 TerminateJobObject 并落 TimedOut。
    let hard_cap = job_hard_cap();
    let deadline_ms = match background
        .then(|| parse_background_timeout(&request.input))
        .flatten()
    {
        Some(explicit) => explicit.min(hard_cap.as_millis() as u64),
        None => hard_cap.as_millis() as u64,
    };
    let stop = {
        let natural = async {
            let status = child.wait().await?;
            tree.wait_for_exit().await;
            Ok::<_, std::io::Error>(status)
        };
        let timer = tokio::time::sleep(Duration::from_millis(deadline_ms));
        tokio::select! {
            result = natural => Stop::Natural(result),
            _ = cancellation.cancelled() => Stop::Cancelled,
            _ = timer => Stop::Deadline,
        }
    };
    let mut status = match &stop {
        Stop::Natural(Ok(code)) => exit_status(*code),
        Stop::Natural(Err(_)) => JobStatus::Failed,
        Stop::Cancelled => JobStatus::Cancelled,
        Stop::Deadline => JobStatus::TimedOut,
    };
    let mut exit_code = match &stop {
        Stop::Natural(Ok(code)) => code.code(),
        _ => None,
    };
    if !matches!(stop, Stop::Natural(Ok(_))) {
        // Cancellation racing an already-completed process is not rollback.
        if tree.is_stopped() {
            if let Some(code) = child.try_wait()? {
                status = exit_status(code);
                exit_code = code.code();
            }
        } else {
            tree.terminate();
            let cleanup = tokio::time::timeout(Duration::from_secs(10), async {
                let code = child.wait().await?;
                tree.wait_for_exit().await;
                Ok::<_, std::io::Error>(code)
            })
            .await;
            match cleanup {
                Ok(Ok(code)) => exit_code = code.code(),
                _ => {
                    return Ok(failure_outcome(
                        JobStatus::RecoveryRequired,
                        "process_exit_unconfirmed",
                        "Termination was requested but process-tree exit is not confirmed.",
                    ))
                }
            }
        }
    }
    // 进程结束后等排水任务收尾，保证预览看到最终（可能截断）的日志。
    let mut logs_truncated = false;
    let mut logs_incomplete = false;
    for mut drain in [stdout_drain, stderr_drain].into_iter().flatten() {
        match tokio::time::timeout(Duration::from_secs(10), &mut drain).await {
            Ok(Ok(Ok(truncated))) => logs_truncated |= truncated,
            _ => {
                drain.abort();
                logs_incomplete = true;
                tracing::warn!(invocation_id = %request.invocation_id, "log drain did not finish cleanly");
            }
        }
    }
    let mut output = preview(
        &stdout_path,
        &stderr_path,
        request.invocation_id,
        evidence_status(status),
        exit_code,
    )
    .await?;
    if logs_truncated {
        tracing::warn!(
            invocation_id = %request.invocation_id,
            limit_bytes = BASH_LOG_STREAM_LIMIT_BYTES,
            "bash log stream hit the per-stream cap and was truncated on disk"
        );
        // 回执必须如实标注落盘截断（P1-1）：模型不能再期待日志文件里有完整输出。
        output.text.push_str(&format!(
            "\n[Log truncated on disk at {} bytes per stream; the command kept running and the remainder was not recorded]",
            BASH_LOG_STREAM_LIMIT_BYTES
        ));
    }
    if logs_incomplete {
        output
            .text
            .push_str("\n[Log capture did not finish cleanly]");
    }
    if let Some(evidence) = output.execution.as_mut() {
        evidence.output_truncated |= logs_truncated || logs_incomplete;
    }
    Ok(Outcome {
        status,
        output: Some(output),
        failure: None,
    })
}

fn exit_status(code: ExitStatus) -> JobStatus {
    if code.success() {
        JobStatus::Completed
    } else {
        JobStatus::Failed
    }
}

fn evidence_status(status: JobStatus) -> ToolExecutionStatus {
    match status {
        JobStatus::Completed => ToolExecutionStatus::Completed,
        JobStatus::Failed => ToolExecutionStatus::Failed,
        JobStatus::Cancelled => ToolExecutionStatus::Cancelled,
        JobStatus::TimedOut => ToolExecutionStatus::TimedOut,
        JobStatus::Running | JobStatus::Queued => ToolExecutionStatus::Running,
        JobStatus::RecoveryRequired => ToolExecutionStatus::Unknown,
    }
}

pub(crate) fn failure_outcome(status: JobStatus, code: &str, message: &str) -> Outcome {
    Outcome {
        status,
        output: None,
        failure: Some(JobFailure {
            code: code.into(),
            message: message.into(),
        }),
    }
}

pub(crate) async fn with_live_preview(mut job: JobSnapshot) -> Result<JobSnapshot> {
    if job.output.is_none() && !job.status.is_terminal() {
        if let (Some(stdout), Some(stderr)) = (&job.stdout_path, &job.stderr_path) {
            job.output = Some(
                preview(
                    &PathBuf::from(stdout),
                    &PathBuf::from(stderr),
                    job.invocation_id,
                    evidence_status(job.status),
                    None,
                )
                .await?,
            );
        }
    }
    Ok(job)
}

async fn preview(
    stdout: &Path,
    stderr: &Path,
    id: uuid::Uuid,
    status: ToolExecutionStatus,
    exit_code: Option<i32>,
) -> Result<ToolOutput> {
    let (out, out_truncated, out_lossy) = read_bounded(stdout, BASH_STDOUT_PREVIEW_BYTES).await?;
    let (err, err_truncated, err_lossy) = read_bounded(stderr, BASH_STDERR_PREVIEW_BYTES).await?;
    let mut text = out;
    if !err.is_empty() {
        text.push_str("\n[stderr]\n");
        text.push_str(&err);
    }
    if text.is_empty() {
        text = "[no output captured]".into();
    }
    if out_truncated || err_truncated {
        text.push_str(&format!(
            "\n[Output truncated. Full stdout: {}; full stderr: {}]",
            stdout.display(),
            stderr.display()
        ));
    }
    Ok(ToolOutput::with_execution(
        text,
        ToolExecutionEvidence {
            status,
            exit_code,
            output_ref: Some(stdout.to_string_lossy().into_owned()),
            output_truncated: out_truncated || err_truncated,
            task_id: Some(id.to_string()),
            // 编码诚实性：无效 UTF-8 被替换为 U+FFFD 时如实标注，不静默。
            encoding_loss: out_lossy || err_lossy,
        },
    ))
}

async fn read_bounded(path: &Path, limit: usize) -> Result<(String, bool, bool)> {
    let file = tokio::fs::File::open(path).await?;
    let length = file.metadata().await?.len();
    let mut bytes = Vec::with_capacity(limit + 4);
    file.take((limit + 4) as u64)
        .read_to_end(&mut bytes)
        .await?;
    // Drop a scalar cut by the preview bound without reporting encoding loss.
    // Any invalid sequence inside the visible prefix is still reported.
    let source_truncated = length > limit as u64 || bytes.len() > limit;
    let mut prefix = &bytes[..bytes.len().min(limit)];
    if source_truncated {
        if let Err(error) = std::str::from_utf8(prefix) {
            if error.error_len().is_none() {
                prefix = &prefix[..error.valid_up_to()];
            }
        }
    }
    let text = String::from_utf8_lossy(prefix);
    let lossy = matches!(text, std::borrow::Cow::Owned(_));
    Ok((truncate_bytes(&text, limit), source_truncated, lossy))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn logs_keep_the_truncation_marker_within_the_disk_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout.log");
        let file = tokio::fs::File::create(&path).await.unwrap();
        let input = [b'a'; 96];
        assert!(drain_bounded(&input[..], file, 64).await.unwrap());
        let bytes = tokio::fs::read(&path).await.unwrap();
        assert_eq!(bytes.len(), 64);
        assert!(bytes.ends_with(b"[log truncated at 64 bytes]"));
    }

    #[tokio::test]
    async fn preview_distinguishes_a_truncated_scalar_from_invalid_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stdout.log");
        tokio::fs::write(&path, "éé").await.unwrap();
        assert_eq!(
            read_bounded(&path, 3).await.unwrap(),
            ("é".into(), true, false)
        );
        tokio::fs::write(&path, b"x\xff").await.unwrap();
        assert_eq!(
            read_bounded(&path, 10).await.unwrap(),
            ("x�".into(), false, true)
        );
    }
}
