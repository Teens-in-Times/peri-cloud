use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::audit::AuditLog;
use crate::config::DeviceConfig;
use crate::native::{is_shell, NativeTools};
use crate::shell::{failure_outcome, Outcome};
use crate::store::Store;
use crate::{Error, Result};
use peri_acp_types::device_executor::*;

struct Worker {
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

pub struct Executor {
    config: DeviceConfig,
    root: PathBuf,
    store: Store,
    audit: AuditLog,
    retention: AsyncMutex<Option<JoinHandle<()>>>,
    retention_stop: CancellationToken,
    admission: AsyncMutex<()>,
    shutdown_gate: AsyncMutex<()>,
    settlement_gate: AsyncMutex<()>,
    closed: AtomicBool,
    sessions: Mutex<HashMap<Uuid, Arc<NativeTools>>>,
    workers: Mutex<HashMap<Uuid, Worker>>,
    slots: Arc<Semaphore>,
}

async fn cleanup_tool_outputs() {
    if let Ok(removed) =
        tokio::task::spawn_blocking(peri_tool_runtime::output::cleanup_persisted_outputs).await
    {
        if removed > 0 {
            tracing::info!(removed, "cleaned stale persisted tool outputs");
        }
    }
}

impl Executor {
    pub async fn open(root: impl AsRef<Path>, device_name: &str) -> Result<Self> {
        let root = root.as_ref().to_owned();
        let config_root = root.clone();
        let name = device_name.to_owned();
        let config =
            tokio::task::spawn_blocking(move || DeviceConfig::open(&config_root, &name)).await??;
        let store = Store::open(&root).await?;
        if let Err(error) = store.enforce_retention(&root).await {
            tracing::warn!(%error, "startup executor retention failed");
        }
        cleanup_tool_outputs().await;
        let retention_store = store.clone();
        let retention_root = root.clone();
        let retention_stop = CancellationToken::new();
        let cancelled = retention_stop.clone();
        let retention = tokio::spawn(async move {
            let period = std::time::Duration::from_secs(3600);
            let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = cancelled.cancelled() => break,
                    _ = ticker.tick() => {
                        if let Err(error) = retention_store.enforce_retention(&retention_root).await {
                            tracing::warn!(%error, "scheduled executor retention failed");
                        }
                        cleanup_tool_outputs().await;
                    }
                }
            }
        });
        Ok(Self {
            config,
            root: root.clone(),
            store,
            audit: AuditLog::open(&root),
            retention: AsyncMutex::new(Some(retention)),
            retention_stop,
            admission: AsyncMutex::new(()),
            shutdown_gate: AsyncMutex::new(()),
            settlement_gate: AsyncMutex::new(()),
            closed: AtomicBool::new(false),
            sessions: Mutex::new(HashMap::new()),
            workers: Mutex::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(8)),
        })
    }

    pub fn info(&self) -> ExecutorInfo {
        self.config.info.clone()
    }

    pub(crate) fn authenticate(&self, secret: &str) -> bool {
        self.config.verify(secret)
    }

    pub async fn bind_session(&self, id: Uuid, request: OpenSession) -> Result<SessionBinding> {
        let _gate = self.admission.lock().await;
        self.ensure_open()?;
        if !Path::new(&request.workspace).is_absolute() {
            return Err(Error::Invalid("workspace must be an absolute path".into()));
        }
        let canonical = tokio::fs::canonicalize(&request.workspace).await?;
        if !tokio::fs::metadata(&canonical).await?.is_dir() {
            return Err(Error::Invalid("workspace must be a directory".into()));
        }
        let workspace = canonical
            .to_str()
            .ok_or_else(|| Error::Invalid("workspace must be valid UTF-8".into()))?
            .to_owned();
        let workspace_identity = crate::workspace::identity(&workspace).await?;
        let session = self
            .store
            .bind(SessionBinding {
                session_id: id,
                workspace,
                workspace_identity,
            })
            .await?;
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(id)
            .or_insert_with(|| Arc::new(NativeTools::new(session.clone(), &self.root)));
        Ok(session)
    }

    pub async fn tools(&self, id: Uuid) -> Result<Vec<NativeToolDescriptor>> {
        Ok(self.session_tools(id).await?.descriptors())
    }

    pub async fn submit(&self, request: SubmitJob) -> Result<JobReceipt> {
        let _gate = self.admission.lock().await;
        self.ensure_open()?;
        self.settle_finished_workers().await?;
        if let Some(job) = self.store.existing_job(&request).await? {
            return Ok(JobReceipt {
                created: false,
                job,
            });
        }
        let tools = self.session_tools(request.session_id).await?;
        crate::workspace::verify(&tools.binding).await?;
        tools.validate(&request, &self.store).await?;
        let (created, job) = self.store.create_job(&request).await?;
        if !created {
            return Ok(JobReceipt { created, job });
        }
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let store = self.store.clone();
        let root = self.root.clone();
        let slots = self.slots.clone();
        let lease = self.config.execution_lease();
        let audit = self.audit.clone();
        let id = request.invocation_id;
        let handle = tokio::spawn(async move {
            let _lease = lease;
            let started = std::time::Instant::now();
            // 设备侧审计（P1-5）：只记身份与结论，不含参数正文与输出。
            let audit_event_session = request.session_id;
            let audit_event_tool = request.tool.clone();
            audit
                .record(&crate::audit::AuditEvent {
                    ts: chrono::Utc::now().to_rfc3339(),
                    kind: "admitted",
                    session_id: audit_event_session,
                    invocation_id: id,
                    tool: &audit_event_tool,
                    status: None,
                    duration_ms: None,
                })
                .await;
            let slot = tokio::select! {
                biased;
                _ = worker_cancel.cancelled() => None,
                slot = slots.acquire_owned() => slot.ok(),
            };
            let outcome = if slot.is_none() || worker_cancel.is_cancelled() {
                failure_outcome(
                    JobStatus::Cancelled,
                    "cancelled_before_start",
                    "Invocation cancelled before starting.",
                )
            } else {
                match store
                    .update(id, |job| {
                        job.status = JobStatus::Running;
                        job.started_at = Some(Utc::now().to_rfc3339());
                    })
                    .await
                {
                    Err(error) => {
                        tracing::error!(invocation_id = %id, %error, "could not persist invocation start");
                        return;
                    }
                    Ok(_) => run_native(&request, tools, &root, &store, worker_cancel).await,
                }
            };
            let settled_status = outcome.status;
            let settled = store
                .update(id, |job| {
                    job.status = outcome.status;
                    job.output = outcome.output;
                    job.failure = outcome.failure;
                    if outcome.status.is_terminal() {
                        job.finished_at = Some(Utc::now().to_rfc3339());
                    }
                })
                .await;
            if let Err(error) = settled {
                tracing::error!(invocation_id = %id, %error, "could not persist invocation result");
                return;
            }
            audit
                .record(&crate::audit::AuditEvent {
                    ts: chrono::Utc::now().to_rfc3339(),
                    kind: "settled",
                    session_id: audit_event_session,
                    invocation_id: id,
                    tool: &audit_event_tool,
                    status: Some(status_key_plain(settled_status)),
                    duration_ms: Some(started.elapsed().as_millis() as u64),
                })
                .await;
        });
        self.workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, Worker { cancel, handle });
        Ok(JobReceipt { created, job })
    }

    pub async fn job(&self, id: Uuid) -> Result<JobSnapshot> {
        self.settle_finished_workers().await?;
        crate::shell::with_live_preview(self.store.job(id).await?).await
    }

    pub async fn session_jobs(&self, id: Uuid) -> Result<Vec<JobSnapshot>> {
        self.settle_finished_workers().await?;
        self.store.session_jobs(id).await
    }

    /// Binding-scoped project instructions. The workspace identity is checked
    /// again so a replaced/moved directory cannot serve different content.
    pub async fn workspace_instructions(&self, id: Uuid) -> Result<WorkspaceInstructions> {
        let workspace = self.workspace_root(id).await?;
        tokio::task::spawn_blocking(move || crate::workspace::instructions(&workspace)).await?
    }

    pub async fn environment(&self, id: Uuid) -> Result<EnvironmentSnapshot> {
        let tools = self.session_tools(id).await?;
        self.ensure_open()?;
        crate::workspace::verify(&tools.binding).await?;
        let info = self.info();
        let binding = tools.binding.clone();
        Ok(
            tokio::task::spawn_blocking(move || crate::environment::snapshot(&info, binding))
                .await?,
        )
    }

    pub async fn workspace_skills(&self, id: Uuid) -> Result<WorkspaceSkills> {
        let workspace = self.workspace_root(id).await?;
        Ok(tokio::task::spawn_blocking(move || crate::workspace::skills(&workspace)).await?)
    }

    pub async fn workspace_read(
        &self,
        id: Uuid,
        request: WorkspaceReadRequest,
    ) -> Result<WorkspaceFile> {
        let workspace = self.workspace_root(id).await?;
        tokio::task::spawn_blocking(move || crate::workspace::read_skill(&workspace, &request.path))
            .await?
    }

    async fn workspace_root(&self, id: Uuid) -> Result<String> {
        self.ensure_open()?;
        let tools = self.session_tools(id).await?;
        crate::workspace::verify(&tools.binding).await?;
        Ok(tools.binding.workspace.clone())
    }

    pub async fn cancel(&self, id: Uuid) -> Result<JobSnapshot> {
        let _gate = self.admission.lock().await;
        let job = self.store.job(id).await?;
        if job.status == JobStatus::RecoveryRequired {
            return Err(Error::RecoveryRequired);
        }
        if job.status.is_terminal() {
            return Ok(job);
        }
        let job = self
            .store
            .update(id, |job| {
                if !job.status.is_terminal() {
                    job.cancel_requested = true;
                }
            })
            .await?;
        if let Some(worker) = self
            .workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
        {
            worker.cancel.cancel();
        }
        Ok(job)
    }

    pub async fn shutdown(&self) -> Result<()> {
        let _shutdown = self.shutdown_gate.lock().await;
        let ids = {
            let _gate = self.admission.lock().await;
            self.closed.store(true, Ordering::Release);
            let workers = self.workers.lock().unwrap_or_else(|e| e.into_inner());
            // Request all cancellations before any fallible persistence I/O.
            // Handles remain owned by self if this close future is cancelled.
            for worker in workers.values() {
                worker.cancel.cancel();
            }
            workers.keys().copied().collect::<Vec<_>>()
        };
        let mut persistence_error = None;
        for id in ids {
            if let Err(error) = self
                .store
                .update(id, |job| {
                    if !job.status.is_terminal() {
                        job.cancel_requested = true;
                    }
                })
                .await
            {
                persistence_error = Some(error);
            }
        }
        loop {
            self.settle_finished_workers().await?;
            if self
                .workers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        if let Some(error) = persistence_error {
            return Err(error);
        }
        self.retention_stop.cancel();
        let mut retention = self.retention.lock().await;
        if let Some(task) = retention.as_mut() {
            let _ = task.await;
            *retention = None;
        }
        self.store.close().await;
        Ok(())
    }

    fn ensure_open(&self) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            Err(Error::ShuttingDown)
        } else {
            Ok(())
        }
    }

    async fn session_tools(&self, id: Uuid) -> Result<Arc<NativeTools>> {
        if let Some(tools) = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned()
        {
            return Ok(tools);
        }
        let binding = self.store.session(id).await?;
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        Ok(sessions
            .entry(id)
            .or_insert_with(|| Arc::new(NativeTools::new(binding, &self.root)))
            .clone())
    }

    async fn settle_finished_workers(&self) -> Result<()> {
        let _settlement = self.settlement_gate.lock().await;
        let finished = {
            let workers = self.workers.lock().unwrap_or_else(|e| e.into_inner());
            workers
                .iter()
                .filter(|(_, w)| w.handle.is_finished())
                .map(|(id, _)| *id)
                .collect::<Vec<_>>()
        };
        for id in finished {
            // Keep the finished owner until its durable outcome is confirmed.
            // A failed store read/update must remain recoverable on a retry.
            let snapshot = self.store.job(id).await?;
            if !snapshot.status.is_terminal() && snapshot.status != JobStatus::RecoveryRequired {
                self.store.update(id, |job| {
                    job.status = JobStatus::RecoveryRequired;
                    job.failure = Some(JobFailure {
                        code: "worker_result_unconfirmed".into(),
                        message: "Worker ended without a durable result. Inspect execution before recovering the session.".into(),
                    });
                }).await?;
            }
            let worker = self
                .workers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            if let Some(worker) = worker {
                if let Err(error) = worker.handle.await {
                    tracing::error!(invocation_id = %id, %error, "executor worker ended unexpectedly");
                }
            }
        }
        Ok(())
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        self.retention_stop.cancel();
        self.closed.store(true, Ordering::Release);
        let workers = self.workers.get_mut().unwrap_or_else(|e| e.into_inner());
        for worker in workers.values() {
            worker.cancel.cancel();
        }
    }
}

async fn run_native(
    request: &SubmitJob,
    tools: Arc<NativeTools>,
    root: &Path,
    store: &Store,
    cancellation: CancellationToken,
) -> Outcome {
    if crate::workspace::verify(&tools.binding).await.is_err() {
        return failure_outcome(JobStatus::Failed, "workspace_changed_before_start", "Bound workspace changed before execution. Create a new session for the current directory.");
    }
    if is_shell(&request.tool) {
        match crate::shell::run(request, &tools.binding.workspace, root, store, cancellation).await {
            Ok(outcome) => outcome,
            Err(_) => failure_outcome(JobStatus::RecoveryRequired, "shell_result_unconfirmed", "Shell execution failed without confirmed final evidence. Inspect the process and logs before recovery."),
        }
    } else {
        match tools
            .invoke_file(request.clone(), store, cancellation)
            .await
        {
            Ok(output) => Outcome {
                status: JobStatus::Completed,
                output: Some(output),
                failure: None,
            },
            Err(message) => failure_outcome(JobStatus::Failed, "native_tool_failed", &message),
        }
    }
}

fn status_key_plain(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Queued => "Queued",
        JobStatus::Running => "Running",
        JobStatus::Completed => "Completed",
        JobStatus::Failed => "Failed",
        JobStatus::Cancelled => "Cancelled",
        JobStatus::TimedOut => "TimedOut",
        JobStatus::RecoveryRequired => "RecoveryRequired",
    }
}
