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
    background: bool,
    parked: bool,
    principal_id: Uuid,
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
    limits: crate::limits::RuntimeLimits,
    journal: Arc<CloudJournal>,
    sessions: RwLock<HashMap<Uuid, Arc<LiveSession>>>,
    ownership: Mutex<Ownership>,
    shutdown_gate: tokio::sync::Mutex<()>,
}

impl CloudRuntime {
    pub fn new(journal: Arc<CloudJournal>) -> Arc<Self> {
        Self::with_limits(journal, crate::limits::RuntimeLimits::default()).expect("valid defaults")
    }

    pub fn with_limits(
        journal: Arc<CloudJournal>,
        limits: crate::limits::RuntimeLimits,
    ) -> Result<Arc<Self>, RuntimeError> {
        if !limits.valid() {
            return Err(StateError::Limit.into());
        }
        Ok(Arc::new(Self {
            limits,
            journal,
            sessions: RwLock::new(HashMap::new()),
            ownership: Mutex::new(Ownership::default()),
            shutdown_gate: tokio::sync::Mutex::new(()),
        }))
    }

    pub fn limits(&self) -> &crate::limits::RuntimeLimits {
        &self.limits
    }
    pub(crate) fn has_active_for(&self, principal: Uuid) -> bool {
        self.ownership.lock().tasks.values().any(|task| {
            task.principal_id == principal && !task.background && !task.handle.is_finished()
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

    /// Rebind an idle, owned session to its newly selected chat origin. Never
    /// replace a broker while a turn, approval or unknown execution is live.
    pub(crate) async fn rebind_idle(
        &self,
        principal: Uuid,
        agent: Arc<CloudAgent>,
        broker: Arc<dyn UserInteractionBroker>,
    ) -> Result<(), RuntimeError> {
        let frozen = agent.frozen_session(principal);
        let state = self.journal.session(principal, frozen.session_id).await?;
        if frozen != state.frozen
            || !state.execution_settled()
            || self.journal.has_unconfirmed_turn(frozen.session_id).await?
        {
            return Err(StateError::Busy.into());
        }
        let owner = self.ownership.lock();
        if owner.closing {
            return Err(RuntimeError::Closing);
        }
        if owner
            .tasks
            .values()
            .any(|task| task.session_id == frozen.session_id && !task.handle.is_finished())
        {
            return Err(StateError::Busy.into());
        }
        self.sessions.write().insert(
            frozen.session_id,
            Arc::new(LiveSession {
                principal_id: principal,
                agent,
                broker,
                classifier: None,
            }),
        );
        Ok(())
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
        self.submit_using(request, None, None).await
    }
    pub(crate) fn has_device_session(&self, session: Uuid) -> bool {
        self.sessions
            .read()
            .get(&session)
            .is_some_and(|s| s.agent.has_device_tools())
    }
    pub(crate) async fn submit_scheduled(
        self: &Arc<Self>,
        request: SubmitTurn,
        agent: Arc<CloudAgent>,
        admission: crate::state::ScheduledAdmission,
    ) -> Result<TurnRecord, RuntimeError> {
        self.submit_using(request, Some(agent), Some(admission))
            .await
    }
    async fn submit_using(
        self: &Arc<Self>,
        request: SubmitTurn,
        scheduled: Option<Arc<CloudAgent>>,
        admission: Option<crate::state::ScheduledAdmission>,
    ) -> Result<TurnRecord, RuntimeError> {
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
        let live = if let Some(agent) = scheduled {
            if agent.frozen_session(request.principal_id)
                != live.agent.frozen_session(request.principal_id)
            {
                return Err(StateError::BindingConflict.into());
            }
            Arc::new(LiveSession {
                principal_id: live.principal_id,
                agent,
                broker: live.broker.clone(),
                classifier: live.classifier.clone(),
            })
        } else {
            live
        };
        if serde_json::to_vec(&request.prompt)
            .map_err(StateError::from)?
            .len()
            > self.limits.max_prompt_bytes
        {
            return Err(StateError::Limit.into());
        }
        // A repeat receipt never starts or charges a second task.
        if let Some(turn) = self
            .journal
            .existing_request(
                request.principal_id,
                request.session_id,
                &request.request_key,
                &request.prompt,
            )
            .await?
        {
            return Ok(turn);
        }
        let task_id = Uuid::new_v4();
        let session_id = request.session_id;
        let principal_id = request.principal_id;
        let background = live.agent.is_worker();
        let cancellation = CancellationToken::new();
        let (sender, receiver) = oneshot::channel();
        {
            let mut owner = self.ownership.lock();
            if owner.closing {
                return Err(RuntimeError::Closing);
            }
            let active: Vec<_> = owner
                .tasks
                .values()
                .filter(|t| !t.handle.is_finished() && !t.parked && t.background == background)
                .collect();
            let total_limit = if background {
                self.limits.max_background_turns
            } else {
                self.limits.max_active_turns
            };
            let user_limit = if background {
                self.limits.max_background_per_user
            } else {
                self.limits.max_active_per_user
            };
            if active.len() >= total_limit
                || active
                    .iter()
                    .filter(|t| t.principal_id == principal_id)
                    .count()
                    >= user_limit
            {
                return Err(StateError::Limit.into());
            }
            let runtime = self.clone();
            let cancel = cancellation.clone();
            // Registration and spawn occur under one synchronous lock. The
            // admission is deployment-owned before its first database await.
            let handle = tokio::spawn(async move {
                runtime
                    .run_owned(task_id, live, request, cancel, sender, admission)
                    .await
            });
            owner.tasks.insert(
                task_id,
                OwnedTask {
                    background,
                    parked: false,
                    principal_id,
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

    pub(crate) fn park_background(&self, principal: Uuid, session: Uuid, turn: Uuid) -> bool {
        let mut owner = self.ownership.lock();
        if owner
            .tasks
            .values()
            .filter(|t| t.parked && !t.handle.is_finished())
            .count()
            >= 64
        {
            return false;
        }
        if let Some(task) = owner.tasks.values_mut().find(|t| {
            t.background
                && t.principal_id == principal
                && t.session_id == session
                && t.turn_id == Some(turn)
                && !t.handle.is_finished()
        }) {
            task.parked = true;
            true
        } else {
            false
        }
    }
    pub(crate) async fn resume_background(
        &self,
        principal: Uuid,
        session: Uuid,
        turn: Uuid,
        cancel: &CancellationToken,
    ) -> bool {
        loop {
            if cancel.is_cancelled() {
                return false;
            }
            {
                let mut owner = self.ownership.lock();
                if owner.closing {
                    return false;
                }
                let key = owner
                    .tasks
                    .iter()
                    .find(|(_, t)| {
                        t.background
                            && t.principal_id == principal
                            && t.session_id == session
                            && t.turn_id == Some(turn)
                    })
                    .map(|(id, _)| *id);
                let Some(key) = key else {
                    return false;
                };
                if !owner.tasks[&key].parked {
                    return true;
                }
                let active: Vec<_> = owner
                    .tasks
                    .values()
                    .filter(|t| t.background && !t.parked && !t.handle.is_finished())
                    .collect();
                if active.len() < self.limits.max_background_turns
                    && active
                        .iter()
                        .filter(|t| t.principal_id == principal)
                        .count()
                        < self.limits.max_background_per_user
                {
                    owner
                        .tasks
                        .get_mut(&key)
                        .expect("owned task under lock")
                        .parked = false;
                    return true;
                }
            }
            tokio::select! { _=cancel.cancelled()=>return false,_=tokio::time::sleep(Duration::from_millis(25))=>() }
        }
    }
    async fn run_owned(
        self: Arc<Self>,
        task_id: Uuid,
        live: Arc<LiveSession>,
        request: SubmitTurn,
        cancellation: CancellationToken,
        receipt: oneshot::Sender<Result<TurnRecord, RuntimeError>>,
        scheduled: Option<crate::state::ScheduledAdmission>,
    ) -> Result<(), RuntimeError> {
        let admission = match self
            .journal
            .admit_with_schedule(
                request.principal_id,
                request.session_id,
                request.request_key,
                request.prompt,
                self.limits.daily_turns,
                scheduled,
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
        let timer_cancel = cancellation.clone();
        let seconds = self.limits.max_turn_seconds;
        let timer = tokio::spawn(async move {
            tokio::select! { _ = timer_cancel.cancelled() => (), _ = tokio::time::sleep(Duration::from_secs(seconds)) => timer_cancel.cancel() }
        });
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
        let result = live
            .agent
            .run_personal_turn(
                run,
                self.journal.clone(),
                request.principal_id,
                &self.limits,
                admission.turn.request_key.starts_with("task-event:"),
            )
            .await;
        timer.abort();
        match result {
            Ok(result) => {
                self.journal.settle(admission.turn.turn_id, &result).await?;
                live.agent.release_settled_device().await;
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
            let principal_id = live.principal_id;
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
                    background: false,
                    parked: false,
                    principal_id,
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
