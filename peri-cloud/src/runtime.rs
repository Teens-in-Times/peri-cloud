use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use peri_acp_types::interaction::UserInteractionBroker;
use peri_acp_types::messages::MessageContent;
use peri_acp_types::permission::{PermissionMode, SharedPermissionMode};
use peri_middlewares::permission::AutoClassifier;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::state::{CloudJournal, StateError, TurnRecord};
use crate::{CloudAgent, CloudTurnRequest};

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error(transparent)]
    State(#[from] StateError),
    #[error("cloud runtime is closing")]
    Closing,
    #[error("cloud session is not attached")]
    NotAttached,
    #[error("cloud session is already attached")]
    AlreadyAttached,
    #[error("cloud turn requires a positive iteration limit")]
    InvalidIterationLimit,
    #[error("cloud owned execution requires recovery")]
    ExecutionUnconfirmed,
}

/// The authenticated gateway supplies identity, not SSH or tool coordinates.
pub struct SubmitTurn {
    pub principal_id: Uuid,
    pub session_id: Uuid,
    pub request_key: String,
    pub prompt: MessageContent,
    pub max_iterations: usize,
}

struct LiveSession {
    principal_id: Uuid,
    agent: Arc<CloudAgent>,
    broker: Arc<dyn UserInteractionBroker>,
    classifier: Option<Arc<dyn AutoClassifier>>,
}

struct OwnedTask {
    session_id: Uuid,
    turn_id: Option<Uuid>,
    cancellation: CancellationToken,
    handle: JoinHandle<Result<(), RuntimeError>>,
}

#[derive(Default)]
struct Ownership {
    closing: bool,
    cleanup_started: bool,
    tasks: HashMap<Uuid, OwnedTask>,
    unconfirmed_sessions: Vec<Uuid>,
}

#[derive(Debug, Clone)]
pub struct ShutdownReport {
    pub complete: bool,
    pub pending_sessions: Vec<Uuid>,
    pub unconfirmed_sessions: Vec<Uuid>,
}

#[derive(Debug, Clone)]
pub struct RuntimeStatus {
    pub closing: bool,
    pub active_sessions: Vec<Uuid>,
}

/// Deployment owns admission and run futures. Cancelling a HTTP/QQ waiter does
/// not drop the loop or the journal transaction; shutdown has a bounded report
/// and retains live handles so a later attempt can observe actual completion.
pub struct CloudRuntime {
    journal: Arc<CloudJournal>,
    sessions: RwLock<HashMap<Uuid, Arc<LiveSession>>>,
    ownership: Mutex<Ownership>,
    shutdown_gate: tokio::sync::Mutex<()>,
}

impl CloudRuntime {
    pub fn new(journal: Arc<CloudJournal>) -> Arc<Self> {
        Arc::new(Self {
            journal,
            sessions: RwLock::new(HashMap::new()),
            ownership: Mutex::new(Ownership::default()),
            shutdown_gate: tokio::sync::Mutex::new(()),
        })
    }

    pub async fn attach(
        &self,
        principal: Uuid,
        agent: Arc<CloudAgent>,
        mode: PermissionMode,
        broker: Arc<dyn UserInteractionBroker>,
        classifier: Option<Arc<dyn AutoClassifier>>,
    ) -> Result<Uuid, RuntimeError> {
        let frozen = agent.frozen_session(principal);
        let id = frozen.session_id;
        if self.ownership.lock().closing {
            return Err(RuntimeError::Closing);
        }
        self.journal.bind(frozen, mode).await?;
        let ownership = self.ownership.lock();
        if ownership.closing {
            return Err(RuntimeError::Closing);
        }
        let mut sessions = self.sessions.write();
        if sessions.contains_key(&id) {
            return Err(RuntimeError::AlreadyAttached);
        }
        sessions.insert(
            id,
            Arc::new(LiveSession {
                principal_id: principal,
                agent,
                broker,
                classifier,
            }),
        );
        Ok(id)
    }

    pub fn journal(&self) -> &Arc<CloudJournal> {
        &self.journal
    }

    pub fn status(&self) -> RuntimeStatus {
        let owner = self.ownership.lock();
        RuntimeStatus {
            closing: owner.closing,
            active_sessions: owner
                .tasks
                .values()
                .filter(|task| !task.handle.is_finished())
                .map(|task| task.session_id)
                .collect(),
        }
    }

    pub async fn submit(self: &Arc<Self>, request: SubmitTurn) -> Result<TurnRecord, RuntimeError> {
        if request.max_iterations == 0 {
            return Err(RuntimeError::InvalidIterationLimit);
        }
        self.reap_finished().await;
        let live = self
            .sessions
            .read()
            .get(&request.session_id)
            .cloned()
            .ok_or(RuntimeError::NotAttached)?;
        if live.principal_id != request.principal_id {
            return Err(StateError::Forbidden.into());
        }
        let task_id = Uuid::new_v4();
        let session_id = request.session_id;
        let cancellation = CancellationToken::new();
        let (sender, receiver) = oneshot::channel();
        {
            let mut owner = self.ownership.lock();
            if owner.closing {
                return Err(RuntimeError::Closing);
            }
            let runtime = self.clone();
            let cancel = cancellation.clone();
            // Registration and spawn occur under one synchronous lock. The
            // admission is deployment-owned before its first database await.
            let handle = tokio::spawn(async move {
                runtime
                    .run_owned(task_id, live, request, cancel, sender)
                    .await
            });
            owner.tasks.insert(
                task_id,
                OwnedTask {
                    session_id,
                    turn_id: None,
                    cancellation,
                    handle,
                },
            );
        }
        receiver
            .await
            .map_err(|_| RuntimeError::ExecutionUnconfirmed)?
    }

    /// Broker waits share the loop's deployment-owned cancellation. A Weak
    /// runtime reference at the broker avoids a live-session ownership cycle.
    pub(crate) fn turn_cancellation(
        &self,
        principal: Uuid,
        session: Uuid,
        turn: Uuid,
    ) -> Option<CancellationToken> {
        if !self
            .sessions
            .read()
            .get(&session)
            .is_some_and(|live| live.principal_id == principal)
        {
            return None;
        }
        self.ownership
            .lock()
            .tasks
            .values()
            .find(|task| task.session_id == session && task.turn_id == Some(turn))
            .map(|task| task.cancellation.clone())
    }

    async fn run_owned(
        self: Arc<Self>,
        task_id: Uuid,
        live: Arc<LiveSession>,
        request: SubmitTurn,
        cancellation: CancellationToken,
        receipt: oneshot::Sender<Result<TurnRecord, RuntimeError>>,
    ) -> Result<(), RuntimeError> {
        let admission = match self
            .journal
            .admit(
                request.principal_id,
                request.session_id,
                request.request_key,
                request.prompt,
            )
            .await
        {
            Ok(admission) => admission,
            Err(error) => {
                let _ = receipt.send(Err(error.into()));
                return Ok(());
            }
        };
        {
            let mut owner = self.ownership.lock();
            if let Some(task) = owner.tasks.get_mut(&task_id) {
                task.turn_id = Some(admission.turn.turn_id);
            }
        }
        let _ = receipt.send(Ok(admission.turn.clone()));
        if !admission.created {
            return Ok(());
        }
        self.journal.start(admission.turn.turn_id).await?;
        // A cancellation may have been persisted while admission was settling.
        if self
            .journal
            .turn(
                request.principal_id,
                request.session_id,
                admission.turn.turn_id,
            )
            .await?
            .cancel_requested
        {
            cancellation.cancel();
        }
        let run = CloudTurnRequest {
            turn_id: admission.turn.turn_id,
            prompt: admission.turn.prompt,
            history: admission.session.history,
            history_flags: admission.session.history_flags,
            broker: live.broker.clone(),
            permissions: SharedPermissionMode::new(PermissionMode::from(
                admission.turn.permission_mode,
            )),
            classifier: live.classifier.clone(),
            cancellation,
            max_iterations: request.max_iterations,
        };
        match live.agent.run_turn(run).await {
            Ok(result) => {
                self.journal.settle(admission.turn.turn_id, &result).await?;
                Ok(())
            }
            Err(_) => {
                self.journal.mark_uncertain(admission.turn.turn_id).await?;
                Err(RuntimeError::ExecutionUnconfirmed)
            }
        }
    }

    pub async fn cancel(
        &self,
        principal: Uuid,
        session: Uuid,
        turn: Uuid,
    ) -> Result<TurnRecord, RuntimeError> {
        let record = self
            .journal
            .request_cancel(principal, session, turn)
            .await?;
        for task in self.ownership.lock().tasks.values() {
            if task.session_id == session && task.turn_id == Some(turn) {
                task.cancellation.cancel();
            }
        }
        Ok(record)
    }

    async fn reap_finished(&self) {
        let finished = {
            let mut owner = self.ownership.lock();
            let ids: Vec<_> = owner
                .tasks
                .iter()
                .filter(|(_, task)| task.handle.is_finished())
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter()
                .filter_map(|id| owner.tasks.remove(&id))
                .collect::<Vec<_>>()
        };
        for task in finished {
            if !matches!(task.handle.await, Ok(Ok(()))) {
                let mut owner = self.ownership.lock();
                if !owner.unconfirmed_sessions.contains(&task.session_id) {
                    owner.unconfirmed_sessions.push(task.session_id);
                }
            }
        }
    }

    fn begin_device_cleanup(self: &Arc<Self>) {
        let mut owner = self.ownership.lock();
        if !owner.tasks.is_empty() || owner.cleanup_started {
            return;
        }
        owner.cleanup_started = true;
        for (session_id, live) in self.sessions.read().iter() {
            let id = *session_id;
            let live = live.clone();
            let journal = self.journal.clone();
            let handle = tokio::spawn(async move {
                let revision = journal.session(live.principal_id, id).await?.revision;
                let jobs = live
                    .agent
                    .cancel_bound_jobs()
                    .await
                    .map_err(|_| RuntimeError::ExecutionUnconfirmed)?;
                journal
                    .reconcile_jobs(live.principal_id, id, revision, jobs)
                    .await?;
                if !journal
                    .session(live.principal_id, id)
                    .await?
                    .execution_settled()
                    || journal.has_unconfirmed_turn(id).await?
                {
                    return Err(RuntimeError::ExecutionUnconfirmed);
                }
                Ok(())
            });
            owner.tasks.insert(
                Uuid::new_v4(),
                OwnedTask {
                    session_id: id,
                    turn_id: None,
                    cancellation: CancellationToken::new(),
                    handle,
                },
            );
        }
    }

    pub async fn shutdown(self: &Arc<Self>, budget: Duration) -> ShutdownReport {
        let _shutdown = self.shutdown_gate.lock().await;
        {
            let mut owner = self.ownership.lock();
            owner.closing = true;
            for task in owner.tasks.values() {
                task.cancellation.cancel();
            }
        }
        let deadline = Instant::now() + budget;
        loop {
            self.reap_finished().await;
            self.begin_device_cleanup();
            let report = {
                let owner = self.ownership.lock();
                ShutdownReport {
                    complete: owner.tasks.is_empty() && owner.unconfirmed_sessions.is_empty(),
                    pending_sessions: owner.tasks.values().map(|task| task.session_id).collect(),
                    unconfirmed_sessions: owner.unconfirmed_sessions.clone(),
                }
            };
            if report.complete {
                self.journal.close().await;
                return report;
            }
            if report.pending_sessions.is_empty() || Instant::now() >= deadline {
                return report;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}
