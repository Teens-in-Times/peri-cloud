use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

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
    admission: AsyncMutex<()>,
    shutdown_gate: AsyncMutex<()>,
    settlement_gate: AsyncMutex<()>,
    closed: AtomicBool,
    sessions: Mutex<HashMap<Uuid, Arc<NativeTools>>>,
    workers: Mutex<HashMap<Uuid, Worker>>,
    slots: Arc<Semaphore>,
}

impl Executor {
    pub async fn open(root: impl AsRef<Path>, device_name: &str) -> Result<Self> {
        let root = root.as_ref().to_owned();
        let config_root = root.clone();
        let name = device_name.to_owned();
        let config =
            tokio::task::spawn_blocking(move || DeviceConfig::open(&config_root, &name)).await??;
        let store = Store::open(&root).await?;
        Ok(Self {
            config,
            root,
            store,
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
            .or_insert_with(|| Arc::new(NativeTools::new(session.clone())));
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
        tools.validate(&request)?;
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
        let id = request.invocation_id;
        let handle = tokio::spawn(async move {
            let _lease = lease;
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
            if let Err(error) = store
                .update(id, |job| {
                    job.status = outcome.status;
                    job.output = outcome.output;
                    job.failure = outcome.failure;
                    if outcome.status.is_terminal() {
                        job.finished_at = Some(Utc::now().to_rfc3339());
                    }
                })
                .await
            {
                tracing::error!(invocation_id = %id, %error, "could not persist invocation result");
            }
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
            .or_insert_with(|| Arc::new(NativeTools::new(binding)))
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
        match tools.invoke_file(request.clone(), cancellation).await {
            Ok(output) => Outcome {
                status: JobStatus::Completed,
                output: Some(output),
                failure: None,
            },
            Err(message) => failure_outcome(JobStatus::Failed, "native_tool_failed", &message),
        }
    }
}
