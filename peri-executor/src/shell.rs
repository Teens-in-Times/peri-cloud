use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use peri_acp_types::tools::{ToolExecutionEvidence, ToolExecutionStatus, ToolOutput};
use peri_process::ProcessTree;
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
    let stdout = tokio::fs::File::create(&stdout_path)
        .await?
        .into_std()
        .await;
    let stderr = tokio::fs::File::create(&stderr_path)
        .await?
        .into_std()
        .await;
    let command = request.input["command"]
        .as_str()
        .ok_or_else(|| Error::Invalid("Bash requires command".into()))?;
    let mut shell = shell_command(command, &[]);
    shell
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
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
    let deadline = background
        .then(|| parse_background_timeout(&request.input))
        .flatten();
    let stop = {
        let natural = async {
            let status = child.wait().await?;
            tree.wait_for_exit().await;
            Ok::<_, std::io::Error>(status)
        };
        let timer = async {
            match deadline {
                Some(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
                None => std::future::pending::<()>().await,
            }
        };
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
    let output = preview(
        &stdout_path,
        &stderr_path,
        request.invocation_id,
        evidence_status(status),
        exit_code,
    )
    .await?;
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
    if job.output.is_none() {
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
    let (out, out_truncated) = read_bounded(stdout, 5_000).await?;
    let (err, err_truncated) = read_bounded(stderr, 3_000).await?;
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
        },
    ))
}

async fn read_bounded(path: &Path, limit: usize) -> Result<(String, bool)> {
    let file = tokio::fs::File::open(path).await?;
    let length = file.metadata().await?.len();
    let mut bytes = Vec::with_capacity(limit + 4);
    file.take((limit + 4) as u64)
        .read_to_end(&mut bytes)
        .await?;
    let text = String::from_utf8_lossy(&bytes);
    let truncated = truncate_bytes(&text, limit);
    let is_truncated = length > truncated.len() as u64;
    Ok((truncated, is_truncated))
}
