use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use parking_lot::RwLock;
use peri_acp_types::device_executor::{JobSnapshot, JobStatus};
use peri_acp_types::event_v2::{EventBus, EventHandles, RenderEvent};
use peri_acp_types::identity::AgentId;
use peri_acp_types::interaction::UserInteractionBroker;
use peri_acp_types::messages::{BaseMessage, MessageContent, MessageId};
use peri_acp_types::permission::SharedPermissionMode;
use peri_acp_types::session::TurnId;
use peri_acp_types::store::{MessageFlags, PersistedPayload};
use peri_agent::agent::compact_v2::config::CompactConfig;
use peri_agent::agent::model_bridge::AgentModelBridge;
use peri_agent::agent::stages::{run_react_loop, LoopResult, StageContext};
use peri_agent::agent::token::ContextBudget;
use peri_agent::session::factory::build_middleware_chain;
use peri_agent::session::tool_catalog::SessionToolCatalog;
use peri_agent::session::{
    FrozenContext, MessageSource, MessageTranscript, QueuedMessage, Session,
};
use peri_agent::tools::BaseTool;
use peri_middlewares::permission::AutoClassifier;
use peri_model::Model;
use peri_remote_tools::RemoteSession;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::assembly::{CloudAssemblyContext, CloudChainAssembler};
use crate::reply::{replies, ChatReply};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cloud session does not match its frozen device binding")]
    SessionMismatch,
    #[error("cloud agent has no valid context window")]
    InvalidContextWindow,
    #[error("cloud turn requires a positive iteration limit")]
    InvalidIterationLimit,
    #[error("cloud tool catalog is ambiguous")]
    ToolCatalog,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Completed,
    Interrupted,
    Failed,
}

/// The deployment persists this turn ID and history before admitting execution.
/// Permission mode and broker belong to the authenticated channel session.
pub struct CloudTurnRequest {
    pub turn_id: Uuid,
    pub prompt: MessageContent,
    pub history: Vec<PersistedPayload>,
    pub history_flags: HashMap<MessageId, MessageFlags>,
    pub broker: Arc<dyn UserInteractionBroker>,
    pub permissions: Arc<SharedPermissionMode>,
    pub classifier: Option<Arc<dyn AutoClassifier>>,
    pub cancellation: CancellationToken,
    pub max_iterations: usize,
}

/// Internal result. A gateway projects `replies`; it cannot serialize this whole
/// object as chat because internal events and canonical history are separate.
pub struct CloudTurnResult {
    pub status: TurnStatus,
    pub replies: Vec<ChatReply>,
    pub history: Vec<PersistedPayload>,
    pub history_flags: HashMap<MessageId, MessageFlags>,
    pub internal_events: Vec<RenderEvent>,
    /// None means the executor could not be queried; empty proves no active jobs.
    pub unsettled_jobs: Option<Vec<JobSnapshot>>,
}

/// Original Peri loop with cloud-owned policy and a frozen native tool backend.
pub struct CloudAgent {
    session_id: Uuid,
    remote: Arc<RemoteSession>,
    model: Arc<dyn Model>,
    frozen_prompt: String,
    context_window: u32,
    turn_gate: Mutex<()>,
}

impl CloudAgent {
    pub fn new(
        session_id: Uuid,
        remote: Arc<RemoteSession>,
        model: Arc<dyn Model>,
        frozen_prompt: String,
        context_window: u32,
    ) -> Result<Self, Error> {
        if remote.cloud_session_id() != session_id.to_string() {
            return Err(Error::SessionMismatch);
        }
        if context_window == 0 {
            return Err(Error::InvalidContextWindow);
        }
        Ok(Self {
            session_id,
            remote,
            model,
            frozen_prompt,
            context_window,
            turn_gate: Mutex::new(()),
        })
    }

    pub async fn run_turn(&self, request: CloudTurnRequest) -> Result<CloudTurnResult, Error> {
        if request.max_iterations == 0 {
            return Err(Error::InvalidIterationLimit);
        }
        let _gate = self.turn_gate.lock().await;
        let assembly = CloudAssemblyContext {
            remote: self.remote.clone(),
            broker: request.broker,
            permissions: request.permissions,
            classifier: request.classifier,
        };
        let chain = Arc::new(build_middleware_chain(&CloudChainAssembler, &assembly));
        let cwd = self.remote.binding().workspace.clone();
        let session = Session::new_with_cancel(
            Arc::from(cwd.as_str()),
            FrozenContext::builder()
                .system_prompt(&self.frozen_prompt)
                .build(),
            None,
            Arc::new(request.cancellation.clone()),
        );
        let initial_ids: HashSet<_> = request.history.iter().map(PersistedPayload::id).collect();
        let transcript = session.transcript();
        {
            let mut current = transcript.write();
            *current = MessageTranscript::new().with_own_payloads(request.history);
            current.set_flags_batch(request.history_flags);
        }
        let mut tool_map = BTreeMap::<String, Arc<dyn BaseTool>>::new();
        for tool in chain.collect_tools(&cwd) {
            if tool_map
                .insert(tool.name().to_owned(), Arc::from(tool))
                .is_some()
            {
                return Err(Error::ToolCatalog);
            }
        }
        let catalog = Arc::new(
            SessionToolCatalog::try_new(tool_map.clone(), None).map_err(|_| Error::ToolCatalog)?,
        );
        let tools = Arc::new(RwLock::new(tool_map));
        let contributions = chain.clone();
        let llm = Arc::new(
            AgentModelBridge::new(self.model.clone())
                .with_system(&self.frozen_prompt)
                .with_session_id(self.session_id.to_string())
                .with_system_contribution_provider(Arc::new(move || {
                    contributions.collect_prompt_contributions()
                })),
        );
        let mut turn = session.start_turn();
        turn.turn_id = TurnId::from_uuid(request.turn_id);
        let (bus, handles) = EventBus::new(Default::default());
        let mut metadata = HashMap::new();
        metadata.insert("session_id".into(), self.session_id.to_string());
        let context = StageContext::builder(turn, transcript.clone(), session.queue().clone())
            .with_llm(llm)
            .with_tools(tools)
            .with_tool_catalog(catalog)
            .with_middleware_chain(chain)
            .with_event_bus(Arc::new(bus))
            .with_session_context(Arc::new(RwLock::new(metadata)))
            .with_agent_id(AgentId::from_uuid(self.session_id))
            .with_context_budget(ContextBudget::new(self.context_window))
            .with_compact_config(CompactConfig::default())
            .with_compact_llm(self.model.clone())
            .build();
        context.session.queue.push(QueuedMessage::prompt(
            MessageSource::UserInput,
            BaseMessage::human(request.prompt),
        ));
        // Scope owns both futures. Events are drained while the original loop
        // runs, then its senders are dropped and the final queued events drain.
        let (outcome, internal_events) = tokio::join!(
            run_react_loop(context, request.max_iterations),
            drain_events(handles)
        );
        let status = match outcome {
            LoopResult::Completed => TurnStatus::Completed,
            LoopResult::Interrupted => TurnStatus::Interrupted,
            LoopResult::Error(error) => {
                // Provider errors may carry request/body data. Log category only.
                tracing::warn!(session_id = %self.session_id, error_category = ?std::mem::discriminant(&error), "cloud turn failed");
                TurnStatus::Failed
            }
        };
        let jobs = if status == TurnStatus::Interrupted {
            self.remote.cancel_active().await
        } else {
            self.remote.jobs().await
        };
        let unsettled_jobs = jobs.ok().map(|jobs| {
            jobs.into_iter()
                .filter(|job| {
                    !job.status.is_terminal() || job.status == JobStatus::RecoveryRequired
                })
                .collect()
        });
        let (history, history_flags) = {
            let current = transcript.read();
            let history = current.persisted_payloads();
            let flags = history
                .iter()
                .map(|payload| (payload.id(), current.flags(payload.id())))
                .collect();
            (history, flags)
        };
        // Compact may replace or reorder the historical view. Identity, rather
        // than an old length, determines which assistant replies are new.
        let new_payloads: Vec<_> = history
            .iter()
            .filter(|payload| !initial_ids.contains(&payload.id()))
            .cloned()
            .collect();
        let visible = replies(&new_payloads);
        Ok(CloudTurnResult {
            status,
            replies: visible,
            history,
            history_flags,
            internal_events,
            unsettled_jobs,
        })
    }

    pub fn frozen_session(&self, principal_id: Uuid) -> crate::state::FrozenSession {
        crate::state::FrozenSession {
            session_id: self.session_id,
            principal_id,
            device_id: self.remote.device_id(),
            binding: self.remote.binding().clone(),
            system_prompt: self.frozen_prompt.clone(),
            context_window: self.context_window,
        }
    }

    pub(crate) async fn cancel_bound_jobs(
        &self,
    ) -> Result<Vec<JobSnapshot>, peri_remote_tools::Error> {
        self.remote.cancel_active().await
    }
}

async fn drain_events(mut handles: EventHandles) -> Vec<RenderEvent> {
    // Internal diagnostics are bounded; canonical history and executor tasks
    // remain the durable sources of execution evidence.
    const RETAINED_DIAGNOSTIC_EVENTS: usize = 512;
    let mut events = VecDeque::new();
    let mut dropped = 0_u64;
    let mut render_open = true;
    let mut state_open = true;
    let mut observe_open = true;
    while render_open || state_open || observe_open {
        tokio::select! {
            event = handles.render_rx.recv(), if render_open => match event {
                Some(event) => {
                    if events.len() == RETAINED_DIAGNOSTIC_EVENTS { events.pop_front(); dropped += 1; }
                    events.push_back(event);
                },
                None => render_open = false
            },
            event = handles.state_rx.recv(), if state_open => if event.is_none() { state_open = false; },
            event = handles.observe_rx.recv(), if observe_open => if matches!(event, Err(tokio::sync::broadcast::error::RecvError::Closed)) { observe_open = false; },
        }
    }
    if dropped > 0 {
        tracing::warn!(dropped, "cloud diagnostic event retention limit reached");
    }
    events.into_iter().collect()
}
