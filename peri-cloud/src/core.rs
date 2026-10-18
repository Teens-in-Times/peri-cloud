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
use peri_middlewares::tool_search::{ExecuteExtraToolResolver, ToolSearchIndex};
use peri_model::Model;
use peri_remote_tools::RemoteSession;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::assembly::{CloudAssemblyContext, CloudChainAssembler, CloudToolSearch, CloudWebTools};
use crate::mcp::McpCache;
use crate::reply::{replies, ChatReply};
use crate::workspace::WorkspaceCache;

#[path = "compact.rs"]
mod compact;

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
    #[error("account model configuration is unavailable")]
    AccountModel,
    #[error("account memory is unavailable")]
    AccountMemory,
    #[error("account goal state is unavailable")]
    AccountGoal,
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
    remote: Option<Arc<RemoteSession>>,
    device_id: Option<Uuid>,
    binding: Option<peri_acp_types::device_executor::SessionBinding>,
    scheduled: std::sync::atomic::AtomicBool,
    private_memory: bool,
    model: Arc<dyn Model>,
    frozen_prompt: String,
    context_window: u32,
    turn_gate: Mutex<()>,
    commands: Arc<peri_acp_types::command::CommandRegistry>,
    account_models: Option<Arc<crate::models::AccountModels>>,
    schedule_push: std::sync::atomic::AtomicBool,
    workspace_cache: Arc<WorkspaceCache>,
    mcp_cache: Arc<McpCache>,
}

impl CloudAgent {
    pub(crate) fn has_device_tools(&self) -> bool {
        self.remote.is_some()
    }
    pub(crate) fn for_scheduled(&self) -> Arc<Self> {
        Arc::new(Self {
            session_id: self.session_id,
            remote: self.remote.clone(),
            device_id: self.device_id,
            binding: self.binding.clone(),
            scheduled: std::sync::atomic::AtomicBool::new(true),
            private_memory: self.private_memory,
            model: self.model.clone(),
            frozen_prompt: self.frozen_prompt.clone(),
            context_window: self.context_window,
            turn_gate: Mutex::new(()),
            commands: self.commands.clone(),
            account_models: self.account_models.clone(),
            schedule_push: std::sync::atomic::AtomicBool::new(
                self.schedule_push
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
            workspace_cache: self.workspace_cache.clone(),
            mcp_cache: self.mcp_cache.clone(),
        })
    }
    pub(crate) fn for_memory_visibility(&self, private: bool) -> Arc<Self> {
        let mut cloned = self.for_scheduled();
        let agent = Arc::get_mut(&mut cloned).expect("new agent clone");
        agent.private_memory = private;
        agent.scheduled.store(
            self.scheduled.load(std::sync::atomic::Ordering::Relaxed),
            std::sync::atomic::Ordering::Relaxed,
        );
        cloned
    }
    pub(crate) fn conversation(
        frozen: &crate::state::FrozenSession,
        model: Arc<dyn Model>,
    ) -> Self {
        Self {
            session_id: frozen.session_id,
            remote: None,
            device_id: frozen.device_id,
            binding: frozen.binding.clone(),
            scheduled: std::sync::atomic::AtomicBool::new(false),
            private_memory: true,
            model,
            frozen_prompt: frozen.system_prompt.clone(),
            context_window: frozen.context_window,
            turn_gate: Mutex::new(()),
            commands: crate::commands::native_registry(),
            account_models: None,
            schedule_push: std::sync::atomic::AtomicBool::new(false),
            workspace_cache: Arc::new(WorkspaceCache::new()),
            mcp_cache: Arc::new(McpCache::new()),
        }
    }
    pub(crate) fn configure_schedule_notifications(&self, push: bool) {
        self.schedule_push
            .store(push, std::sync::atomic::Ordering::Relaxed);
    }
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
            device_id: Some(remote.device_id()),
            binding: Some(remote.binding().clone()),
            remote: Some(remote),
            scheduled: std::sync::atomic::AtomicBool::new(false),
            private_memory: true,
            model,
            frozen_prompt,
            context_window,
            turn_gate: Mutex::new(()),
            commands: crate::commands::native_registry(),
            account_models: None,
            schedule_push: std::sync::atomic::AtomicBool::new(false),
            workspace_cache: Arc::new(WorkspaceCache::new()),
            mcp_cache: Arc::new(McpCache::new()),
        })
    }

    pub fn with_commands(
        mut self,
        commands: Arc<peri_acp_types::command::CommandRegistry>,
    ) -> Self {
        self.commands = commands;
        self
    }

    pub fn with_account_models(mut self, models: Arc<crate::models::AccountModels>) -> Self {
        self.account_models = Some(models);
        self
    }

    pub async fn run_turn(&self, request: CloudTurnRequest) -> Result<CloudTurnResult, Error> {
        self.run_context_turn(request, &self.frozen_prompt, self.model.clone(), None)
            .await
    }

    pub(crate) async fn run_personal_turn(
        &self,
        mut request: CloudTurnRequest,
        journal: Arc<crate::state::CloudJournal>,
        principal: Uuid,
        limits: &crate::limits::RuntimeLimits,
    ) -> Result<CloudTurnResult, Error> {
        let prompt = format!("{}\n\n{}", self.frozen_prompt, crate::memory::frames::RULES);
        let mut scope = crate::personal::PersonalScope {
            session_id: self.session_id,
            turn_id: request.turn_id,
            device_id: self.device_id,
            private_memory: self.private_memory,
            memory_fence: Default::default(),
            model_revision: String::new(),
            schedule_push: self
                .schedule_push
                .load(std::sync::atomic::Ordering::Relaxed),
            scheduled: self.scheduled.load(std::sync::atomic::Ordering::Relaxed),
            journal: journal.clone(),
            principal,
            max_bytes: limits.max_memory_bytes,
        };
        let context_only = matches!(&request.prompt, MessageContent::Text(text)
            if self.commands.resolve(text).is_some_and(|command|matches!(command.entry.fullname.as_str(), "core:clear"|"core:rewind")));
        let inner = match &self.account_models {
            Some(_) if context_only => self.model.clone(),
            Some(models) => models
                .resolve_for_turn(
                    principal,
                    self.session_id,
                    request.turn_id,
                    self.model.clone(),
                )
                .await
                .map_err(|_| Error::AccountModel)?,
            None => self.model.clone(),
        };
        scope.model_revision =
            crate::memory::frames::model_revision(inner.as_ref(), self.context_window);
        if self.private_memory && !context_only {
            crate::memory::frames::prepare(&scope, &mut request)
                .await
                .map_err(|_| Error::AccountMemory)?;
        }
        let model = Arc::new(crate::personal::MeteredModel {
            inner,
            journal,
            principal,
            daily_limit: limits.daily_model_requests,
        });
        let model: Arc<dyn Model> = if self.private_memory {
            Arc::new(crate::memory::frames::GuardedModel {
                inner: model,
                scope: scope.clone(),
            })
        } else {
            model
        };
        self.run_context_turn(request, &prompt, model, Some(scope))
            .await
    }

    async fn run_context_turn(
        &self,
        mut request: CloudTurnRequest,
        prompt: &str,
        model: Arc<dyn Model>,
        personal: Option<crate::personal::PersonalScope>,
    ) -> Result<CloudTurnResult, Error> {
        if request.max_iterations == 0 {
            return Err(Error::InvalidIterationLimit);
        }
        let _gate = self.turn_gate.lock().await;
        if let MessageContent::Text(text) = &request.prompt {
            if text.starts_with('/') {
                if let Some(resolved) = self.commands.resolve(text) {
                    let parsed = resolved
                        .entry
                        .args_schema
                        .as_ref()
                        .map(|schema| crate::commands::parse_arguments(schema, &resolved.args));
                    if parsed.as_ref().is_some_and(Result::is_err)
                        || (resolved.entry.fullname == "core:loop" && resolved.args.is_empty())
                    {
                        return Ok(self.command_feedback(
                            request,
                            TurnStatus::Failed,
                            "命令参数不正确。发送 /help 查看用法。",
                        ));
                    }
                    if resolved.entry.fullname == "core:compact" {
                        return Ok(self.compact_turn(request, model).await);
                    }
                    if matches!(
                        resolved.entry.fullname.as_str(),
                        "core:clear" | "core:rewind"
                    ) {
                        return Ok(self.history_command(request, &resolved));
                    }
                    let mut dependencies = peri_acp_types::command::DependencyBag::default();
                    if let Some(scope) = &personal {
                        dependencies.insert(
                            std::any::TypeId::of::<crate::commands::CloudCommandScope>(),
                            Arc::new(crate::commands::CloudCommandScope {
                                principal_id: scope.principal,
                                session_id: self.session_id,
                                turn_id: request.turn_id,
                                device_id: self.device_id,
                                mcp_store: Some(Arc::new(crate::mcp::McpServerStore::new(
                                    scope.journal.identity_pool(),
                                    scope.principal,
                                ))),
                            }),
                        );
                    }
                    let mut context = peri_acp_types::command::CommandContext::new(
                        self.session_id.to_string(),
                        request
                            .history
                            .iter()
                            .filter(|p| {
                                !request
                                    .history_flags
                                    .get(&p.id())
                                    .is_some_and(|f| f.excluded)
                            })
                            .filter_map(|p| p.as_message().cloned())
                            .collect(),
                        self.binding
                            .as_ref()
                            .map(|b| b.workspace.clone())
                            .unwrap_or_default(),
                        Arc::new(compact::NoCommandEvents),
                        request.cancellation.clone(),
                        dependencies,
                    );
                    context.raw_text = text.clone();
                    context.args = resolved.args;
                    context.supports_inject = true;
                    context.parsed_args = parsed.and_then(Result::ok);
                    context.auxiliary_model = Some(model.clone());
                    let result = tokio::select! { biased;
                        _ = request.cancellation.cancelled() => None,
                        result = resolved.entry.handler.execute(context) => Some(result),
                    };
                    match result {
                        Some(peri_acp_types::command::CommandOutcome::Inject(text)) => {
                            request.prompt = MessageContent::Text(text)
                        }
                        Some(peri_acp_types::command::CommandOutcome::Done(result)) => {
                            return Ok(self.handler_result(request, result));
                        }
                        Some(peri_acp_types::command::CommandOutcome::Delegate(_)) => {
                            return Ok(self.command_feedback(
                                request,
                                TurnStatus::Failed,
                                "命令的目标执行器尚未接入，未执行或转交给模型。",
                            ))
                        }
                        None => {
                            return Ok(self.command_feedback(
                                request,
                                TurnStatus::Interrupted,
                                "命令已取消；状态请通过 /status 核对。",
                            ))
                        }
                    }
                } else {
                    return Ok(self.command_feedback(
                        request,
                        TurnStatus::Failed,
                        "命令未注册或已断开，本次没有交给模型或自动重试。发送 /help 查看最新目录。",
                    ));
                }
            }
        }
        // Device-backed instructions and skills are frozen once per bound
        // session. A failed read yields no workspace context, never a cloud
        // host fallback.
        let workspace = match &self.remote {
            Some(remote) => Some(self.workspace_cache.load(remote).await),
            None => None,
        };
        // MCP servers are account-scoped. The cache checks the stored revision
        // each turn and rebuilds the pool when the account's list changes.
        let mcp_pool = match &personal {
            Some(scope) => {
                let store =
                    crate::mcp::McpServerStore::new(scope.journal.identity_pool(), scope.principal);
                self.mcp_cache
                    .load(&store, scope.principal)
                    .await
                    .map(|session| session.pool.clone())
            }
            None => None,
        };
        // A controller exists for every durable session, even before a goal is
        // created: it is what exposes the deferred `goal` tool in the first
        // place. Sharing the tool map with ToolSearch lets it index that tool.
        let goal = match &personal {
            Some(scope) => Some(
                crate::goal::CloudGoalController::load(
                    scope.journal.identity_pool(),
                    self.session_id,
                    scope.principal,
                )
                .await
                .map_err(|_| Error::AccountGoal)?,
            ),
            None => None,
        };
        let tools = Arc::new(RwLock::new(BTreeMap::<String, Arc<dyn BaseTool>>::new()));
        // ExecuteExtraTool holds a strong reference to this map, which contains
        // the tool itself. Clearing after the turn breaks that cycle so the
        // account scopes held by tools (memory, schedules) are released.
        let shared_tools = tools.clone();
        let tool_search = (goal.is_some() || mcp_pool.is_some()).then(|| CloudToolSearch {
            index: Arc::new(ToolSearchIndex::new()),
            tools: tools.clone(),
        });
        let assembly = CloudAssemblyContext {
            remote: self.remote.clone(),
            broker: request.broker,
            permissions: request.permissions,
            classifier: request.classifier,
            personal: personal.clone(),
            web: CloudWebTools::from_env(),
            goal: goal.clone(),
            tool_search,
            workspace,
            mcp_pool,
            session_id: self.session_id.to_string(),
        };
        let mut chain = build_middleware_chain(&CloudChainAssembler, &assembly);
        let cwd = self
            .binding
            .as_ref()
            .map(|b| b.workspace.clone())
            .unwrap_or_default();
        let session = Session::new_with_cancel(
            Arc::from(cwd.as_str()),
            FrozenContext::builder().system_prompt(prompt).build(),
            None,
            Arc::new(request.cancellation.clone()),
        );
        let initial_ids: HashSet<_> = request.history.iter().map(PersistedPayload::id).collect();
        let transcript = session.transcript();
        {
            let mut current = transcript.write();
            *current = MessageTranscript::new().with_own_payloads(request.history);
            // The cloud journal is the canonical store. Keep archived entries in
            // the hot transcript so the end-of-turn settlement preserves them.
            current.enable_deployment_owned_compaction();
            current.set_flags_batch(request.history_flags);
        }
        if let Some(scope) = personal.filter(|s| s.private_memory) {
            chain.add(Box::new(crate::memory::frames::MemoryFrames {
                scope,
                transcript: transcript.clone(),
            }));
        }
        let chain = Arc::new(chain);
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
        *tools.write() = std::mem::take(&mut tool_map);
        let contributions = chain.clone();
        let llm = Arc::new(
            AgentModelBridge::new(model.clone())
                .with_system(prompt)
                .with_session_id(self.session_id.to_string())
                .with_system_contribution_provider(Arc::new(move || {
                    contributions.collect_prompt_contributions()
                })),
        );
        let turn = session
            .start_turn()
            .with_turn_id(TurnId::from_uuid(request.turn_id));
        let (bus, handles) = EventBus::new(Default::default());
        let mut metadata = HashMap::new();
        metadata.insert("session_id".into(), self.session_id.to_string());
        let mut builder = StageContext::builder(turn, transcript.clone(), session.queue().clone())
            .with_llm(llm)
            .with_tools(tools)
            .with_tool_catalog(catalog)
            .with_middleware_chain(chain)
            .with_event_bus(Arc::new(bus))
            .with_session_context(Arc::new(RwLock::new(metadata)))
            .with_agent_id(AgentId::from_uuid(self.session_id))
            .with_context_budget(ContextBudget::new(self.context_window))
            .with_compact_config(CompactConfig::default())
            .with_compact_llm(model)
            // Deferred tools are executed through ExecuteExtraTool. Without
            // this resolver the outer meta tool would be the only approval
            // subject, so `mcp__*` and other deferred tools could bypass HITL.
            .with_tool_invocation_resolver(Arc::new(ExecuteExtraToolResolver::default()));
        if let Some(goal) = &goal {
            builder = builder.with_goal_controller(goal.clone());
        }
        let context = builder.build();
        context.session.queue.push(QueuedMessage::prompt(
            if self.scheduled.load(std::sync::atomic::Ordering::Relaxed) {
                MessageSource::CronTrigger
            } else {
                MessageSource::UserInput
            },
            BaseMessage::human(request.prompt),
        ));
        // Scope owns both futures. Events are drained while the original loop
        // runs, then its senders are dropped and the final queued events drain.
        let (outcome, internal_events) = tokio::join!(
            run_react_loop(context, request.max_iterations),
            drain_events(handles)
        );
        shared_tools.write().clear();
        let status = match outcome {
            LoopResult::Completed => TurnStatus::Completed,
            LoopResult::Interrupted => TurnStatus::Interrupted,
            LoopResult::Error(error) => {
                // Provider errors may carry request/body data. Log category only.
                tracing::warn!(session_id = %self.session_id, error_category = ?std::mem::discriminant(&error), "cloud turn failed");
                TurnStatus::Failed
            }
        };
        let jobs = match &self.remote {
            Some(remote) if status == TurnStatus::Interrupted => remote.cancel_active().await,
            Some(remote) => remote.jobs().await,
            None => Ok(Vec::new()), // Conversation mode has no executor tools; prior jobs were settled at admission.
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
            let history = current.canonical_payloads();
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
            device_id: self.device_id,
            binding: self.binding.clone(),
            system_prompt: self.frozen_prompt.clone(),
            context_window: self.context_window,
        }
    }

    pub(crate) async fn bound_jobs(&self) -> Result<Vec<JobSnapshot>, peri_remote_tools::Error> {
        match &self.remote {
            Some(remote) => remote.jobs().await,
            None => Ok(Vec::new()),
        }
    }

    pub(crate) async fn cancel_bound_jobs(
        &self,
    ) -> Result<Vec<JobSnapshot>, peri_remote_tools::Error> {
        match &self.remote {
            Some(remote) => remote.cancel_active().await,
            None => Ok(Vec::new()),
        }
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
