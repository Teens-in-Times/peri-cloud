use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use peri_acp_types::device_executor::*;
use peri_acp_types::tools::{
    BaseTool, ToolContext, ToolExecutionEvidence, ToolExecutionStatus, ToolOutput,
};
use peri_tool_runtime::shell_contract::parse_foreground_timeout_for_platform;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::tool::{ControlTool, RemoteTool};
use crate::{DeviceClient, Error, Result};

pub struct RemoteSession {
    pub(crate) client: Arc<DeviceClient>,
    pub(crate) binding: SessionBinding,
    cloud_session_id: String,
    catalog: Vec<NativeToolDescriptor>,
    admission: Mutex<()>,
    // true means a submit may have reached the executor. A missing job cannot
    // settle it: the original request may still be in admission after a timeout.
    uncertain: Mutex<HashMap<Uuid, Uncertain>>,
}

#[derive(Default)]
struct Uncertain {
    submitted: bool,
    request_hash: Option<[u8; 32]>,
}

impl RemoteSession {
    pub async fn open(
        client: Arc<DeviceClient>,
        session: Uuid,
        cloud_session_id: String,
        workspace: &str,
    ) -> Result<Arc<Self>> {
        if cloud_session_id.is_empty() {
            return Err(Error::MissingIdentity);
        }
        let binding = client.bind(session, workspace).await?;
        let catalog = client.catalog(session).await?;
        // Initial native protocol supports exactly these tools. Reject a
        // malformed/ambiguous catalog instead of inventing local definitions.
        for name in ["Read", "Write", "Edit", "Glob", "Grep", "Bash"] {
            let matches: Vec<_> = catalog
                .iter()
                .filter(|tool| tool.definition.name == name)
                .collect();
            if matches.len() != 1 || !matches[0].is_direct {
                return Err(Error::Protocol);
            }
            let expected: &[&str] = match name {
                "Read" => &["reading"],
                "Bash" => &["Shell"],
                _ => &[],
            };
            if matches[0]
                .aliases
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                != expected
            {
                return Err(Error::Protocol);
            }
        }
        if catalog.len() != 6 {
            return Err(Error::Protocol);
        }
        Ok(Arc::new(Self {
            client,
            binding,
            cloud_session_id,
            catalog,
            admission: Mutex::new(()),
            uncertain: Mutex::new(HashMap::new()),
        }))
    }

    pub fn binding(&self) -> &SessionBinding {
        &self.binding
    }

    pub fn cloud_session_id(&self) -> &str {
        &self.cloud_session_id
    }

    pub fn device_id(&self) -> Uuid {
        self.client.info().device_id
    }

    /// Request cancellation for this frozen session, preserving uncertainty.
    pub async fn cancel_active(&self) -> Result<Vec<JobSnapshot>> {
        for job in self.jobs().await? {
            if !job.status.is_terminal() && job.status != JobStatus::RecoveryRequired {
                self.client
                    .cancel(self.binding.session_id, job.invocation_id)
                    .await?;
            }
        }
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            let jobs = self.jobs().await?;
            if jobs
                .iter()
                .all(|job| job.status.is_terminal() || job.status == JobStatus::RecoveryRequired)
                || Instant::now() >= deadline
            {
                return Ok(jobs);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub fn tools(self: &Arc<Self>) -> Vec<Box<dyn BaseTool>> {
        let mut tools: Vec<Box<dyn BaseTool>> = self
            .catalog
            .iter()
            .map(|descriptor| {
                Box::new(RemoteTool {
                    session: self.clone(),
                    descriptor: descriptor.clone(),
                }) as Box<dyn BaseTool>
            })
            .collect();
        tools.push(Box::new(ControlTool {
            session: self.clone(),
            cancel: false,
        }));
        tools.push(Box::new(ControlTool {
            session: self.clone(),
            cancel: true,
        }));
        tools
    }

    pub async fn jobs(&self) -> Result<Vec<JobSnapshot>> {
        let jobs = self.client.jobs(self.binding.session_id).await?;
        let mut uncertain = self.uncertain.lock().await;
        uncertain.retain(|id, pending| {
            jobs.iter()
                .find(|job| job.invocation_id == *id)
                .map_or(pending.submitted, |job| !job.status.is_terminal())
        });
        if uncertain
            .keys()
            .any(|id| !jobs.iter().any(|job| job.invocation_id == *id))
        {
            return Err(Error::UnconfirmedExecution);
        }
        Ok(jobs)
    }

    pub(crate) async fn task(&self, id: Uuid, cancel: bool) -> Result<JobSnapshot> {
        let result = if cancel {
            self.client.cancel(self.binding.session_id, id).await
        } else {
            self.client.job(self.binding.session_id, id).await
        };
        let mut uncertain = self.uncertain.lock().await;
        match &result {
            Ok(job) if job.status.is_terminal() => {
                uncertain.remove(&id);
            }
            Err(Error::Rejected(404))
                if uncertain.get(&id).is_some_and(|pending| !pending.submitted) =>
            {
                uncertain.remove(&id);
            }
            Err(error) if ambiguous(error) => {
                uncertain.entry(id).or_default().submitted = true;
            }
            _ => (),
        }
        result
    }

    async fn mark_uncertain(&self, id: Uuid, submitted: bool) {
        self.uncertain
            .lock()
            .await
            .entry(id)
            .and_modify(|existing| existing.submitted |= submitted)
            .or_insert(Uncertain {
                submitted,
                request_hash: None,
            });
    }

    async fn record_request(&self, request: &SubmitJob) -> Result<()> {
        let hash: [u8; 32] =
            Sha256::digest(serde_json::to_vec(request).map_err(|_| Error::Protocol)?).into();
        let mut uncertain = self.uncertain.lock().await;
        let pending = uncertain.entry(request.invocation_id).or_default();
        if pending
            .request_hash
            .is_some_and(|existing| existing != hash)
        {
            return Err(Error::Rejected(409));
        }
        pending.request_hash = Some(hash);
        pending.submitted = true;
        Ok(())
    }

    pub(crate) fn check_context(&self, ctx: &ToolContext<'_>) -> Result<()> {
        match ctx.session_id.as_deref() {
            Some(id) if id == self.cloud_session_id => Ok(()),
            Some(_) => Err(Error::SessionMismatch),
            None => Err(Error::MissingIdentity),
        }
    }

    fn invocation_id(&self, ctx: &ToolContext<'_>) -> Result<Uuid> {
        self.check_context(ctx)?;
        let turn = ctx
            .turn_generation
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or(Error::MissingIdentity)?;
        let invocation = ctx
            .invocation_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or(Error::MissingIdentity)?;
        // The host must persist its turn identity to retain this key across
        // restart. Inputs are excluded: changed input must conflict, not replay.
        let identity = serde_json::to_vec(&(
            self.binding.session_id,
            &self.cloud_session_id,
            turn,
            invocation,
        ))
        .map_err(|_| Error::Protocol)?;
        let digest = Sha256::digest(identity);
        let mut bytes = [0_u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x80;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Ok(Uuid::from_bytes(bytes))
    }

    pub(crate) async fn invoke(
        &self,
        tool: &str,
        input: serde_json::Value,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput> {
        let id = self.invocation_id(&ctx)?;
        // Serialize admission through the result boundary. Parallel provider
        // calls cannot pass this gate before a lost receipt has been recorded.
        // Control queries/cancellation remain available while a call is waiting.
        let _admission = self.admission.lock().await;
        let previously_uncertain = {
            let uncertain = self.uncertain.lock().await;
            if let Some(other) = uncertain.keys().filter(|other| **other != id).min() {
                return Ok(pending_output(*other, ToolExecutionStatus::Unknown,
                    "A prior invocation is unconfirmed. This new operation was not submitted. Query GetExecution with this task ID until it is settled before requesting another operation."));
            }
            uncertain.contains_key(&id)
        };
        let result = self.invoke_bound(id, tool, input, ctx).await;
        if let Ok(output) = &result {
            if output.execution.as_ref().is_some_and(|evidence| {
                !matches!(evidence.status, ToolExecutionStatus::Unknown)
                    && (!previously_uncertain
                        || matches!(
                            evidence.status,
                            ToolExecutionStatus::Completed
                                | ToolExecutionStatus::Failed
                                | ToolExecutionStatus::Cancelled
                                | ToolExecutionStatus::TimedOut
                        ))
            }) {
                self.uncertain.lock().await.remove(&id);
            }
        }
        result
    }

    async fn invoke_bound(
        &self,
        id: Uuid,
        tool: &str,
        input: serde_json::Value,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput> {
        let wait = if tool == "Bash" {
            Duration::from_millis(
                parse_foreground_timeout_for_platform(
                    &input,
                    self.client.info().platform == "windows",
                )
                .0,
            )
        } else {
            Duration::from_secs(120)
        };
        let background = tool == "Bash" && input["run_in_background"].as_bool().unwrap_or(false);
        let request = SubmitJob {
            invocation_id: id,
            session_id: self.binding.session_id,
            tool: tool.into(),
            input,
        };
        let existing = match self.client.job(self.binding.session_id, id).await {
            Ok(job) => Some(job),
            Err(Error::Rejected(404)) if ctx.cancellation.is_cancelled() => {
                if self
                    .uncertain
                    .lock()
                    .await
                    .get(&id)
                    .is_some_and(|pending| pending.submitted)
                {
                    return Ok(disconnected_output(id));
                }
                return Ok(pending_output(
                    id,
                    ToolExecutionStatus::Cancelled,
                    "Invocation was cancelled before admission; no request was submitted.",
                ));
            }
            Err(Error::Rejected(404)) => None,
            Err(error) if ambiguous(&error) => {
                self.mark_uncertain(id, false).await;
                return Ok(disconnected_output(id));
            }
            Err(error) => return Err(error),
        };
        let existing_pending = existing
            .as_ref()
            .is_some_and(|job| !job.status.is_terminal());
        // Record before POST, so dropping this future cannot lose the fact
        // that the executor may still own a committed request. Retain its input
        // identity even when a lost original admission still queries as 404.
        self.record_request(&request).await?;
        // Exact request comparison remains executor-owned, even for terminal
        // invocations. The same ID with changed input still conflicts.
        let job = match self.client.submit(&request).await {
            Ok(receipt) => receipt.job,
            Err(error) if ambiguous(&error) => {
                self.mark_uncertain(id, true).await;
                return Ok(disconnected_output(id));
            }
            Err(error) => {
                if !existing_pending
                    && matches!(error, Error::Rejected(status) if status != 409 || existing.is_some())
                {
                    self.uncertain.lock().await.remove(&id);
                }
                return Err(error);
            }
        };
        if background && !ctx.cancellation.is_cancelled() {
            return Ok(snapshot_output(job));
        }
        self.wait_for_job(id, job, wait, ctx.cancellation).await
    }

    async fn wait_for_job(
        &self,
        id: Uuid,
        mut job: JobSnapshot,
        wait: Duration,
        cancel: CancellationToken,
    ) -> Result<ToolOutput> {
        let mut deadline = Instant::now() + wait;
        let mut requested = false;
        loop {
            if job.status.is_terminal() || job.status == JobStatus::RecoveryRequired {
                return Ok(snapshot_output(job));
            }
            if cancel.is_cancelled() && !requested {
                match self.client.cancel(self.binding.session_id, id).await {
                    Ok(snapshot) => job = snapshot,
                    Err(error) if ambiguous(&error) => {
                        self.mark_uncertain(id, true).await;
                        return Ok(disconnected_output(id));
                    }
                    Err(error) => return Err(error),
                }
                requested = true;
                deadline = Instant::now() + Duration::from_secs(12);
                continue;
            }
            if Instant::now() >= deadline {
                let shell_wait_expired = job.tool == "Bash" && !requested;
                let mut output = snapshot_output(job);
                if shell_wait_expired {
                    if let Some(evidence) = &mut output.execution {
                        evidence.status = ToolExecutionStatus::RunningAfterTimeout;
                    }
                    output.text.push_str(
                        "\nForeground wait expired; the command continues on the executor.",
                    );
                }
                return Ok(output);
            }
            if requested {
                tokio::time::sleep(Duration::from_millis(100)).await;
            } else {
                tokio::select! {
                    _ = cancel.cancelled() => continue,
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
            }
            match self.client.job(self.binding.session_id, id).await {
                Ok(snapshot) => job = snapshot,
                Err(error) if ambiguous(&error) => {
                    self.mark_uncertain(id, true).await;
                    return Ok(disconnected_output(id));
                }
                Err(error) => return Err(error),
            }
        }
    }
}

fn ambiguous(error: &Error) -> bool {
    matches!(
        error,
        Error::Disconnected | Error::Protocol | Error::UnconfirmedExecution
    ) || matches!(error, Error::Rejected(status) if *status == 408 || *status >= 500)
}

pub(crate) fn snapshot_output(job: JobSnapshot) -> ToolOutput {
    let status = match job.status {
        JobStatus::Completed => ToolExecutionStatus::Completed,
        JobStatus::Failed => ToolExecutionStatus::Failed,
        JobStatus::Cancelled => ToolExecutionStatus::Cancelled,
        JobStatus::TimedOut => ToolExecutionStatus::TimedOut,
        JobStatus::Queued | JobStatus::Running => ToolExecutionStatus::Running,
        JobStatus::RecoveryRequired => ToolExecutionStatus::Unknown,
    };
    let message = job.failure.as_ref().map(|failure| failure.message.as_str()).unwrap_or(if job.status.is_terminal() { "Invocation settled without captured output." } else { "Invocation was admitted and is pending or running. Query GetExecution using this task ID; do not repeat the operation." });
    let mut output = job
        .output
        .unwrap_or_else(|| pending_output(job.invocation_id, status, message));
    if output.execution.is_none() {
        output.execution = pending_output(job.invocation_id, status, "").execution;
    }
    if let Some(evidence) = &mut output.execution {
        evidence.status = status;
        evidence.task_id = Some(job.invocation_id.to_string());
    }
    if !job.status.is_terminal() {
        output
            .text
            .push_str("\nQuery GetExecution using the task ID before repeating this operation.");
    }
    output
}

fn disconnected_output(id: Uuid) -> ToolOutput {
    pending_output(id, ToolExecutionStatus::Unknown, "Connection interrupted. Execution is unconfirmed and may still be running. Reconnect and query GetExecution using the task ID; do not submit another operation.")
}

fn pending_output(id: Uuid, status: ToolExecutionStatus, text: &str) -> ToolOutput {
    ToolOutput::with_execution(
        text,
        ToolExecutionEvidence {
            status,
            exit_code: None,
            output_ref: None,
            output_truncated: false,
            task_id: Some(id.to_string()),
        },
    )
}
