use super::store::TaskStore;
use super::*;
use crate::gateway::Gateway;
use crate::state::{CloudJournal, TurnState};
use serde_json::json;
use std::sync::Arc;

pub(crate) struct WorkerService {
    pub(super) gateway: Weak<Gateway>,
    pub(super) journal: Arc<CloudJournal>,
    pub(super) store: TaskStore,
    cursor: std::sync::atomic::AtomicI64,
    waiters: parking_lot::Mutex<std::collections::HashMap<Uuid, Arc<tokio::sync::Notify>>>,
    transcripts: parking_lot::RwLock<
        std::collections::HashMap<
            Uuid,
            Weak<parking_lot::RwLock<peri_agent::session::MessageTranscript>>,
        >,
    >,
}
impl WorkerService {
    pub(crate) async fn record(&self, principal: Uuid, id: Uuid) -> Result<TaskRecord> {
        self.store.get(principal, id).await
    }
    pub(super) async fn request_cancel(
        &self,
        scope: &TaskScope,
        id: Uuid,
        revision: u64,
    ) -> Result<TaskRecord> {
        let gateway = self.gateway.upgrade().ok_or(TaskError::Unavailable)?;
        let gate = gateway.worker_control_gate(scope.principal);
        let _guard = gate.lock().await;
        let mut record = self.authorized(scope, id).await?;
        if record.revision != revision {
            return Err(TaskError::Conflict);
        }
        if record.terminal() {
            return Ok(record);
        }
        if let Some(turn) = gateway
            .worker_admission(&record)
            .await
            .map_err(|_| TaskError::Unavailable)?
        {
            gateway
                .cancel_worker(&record, turn.turn_id)
                .await
                .map_err(|_| TaskError::Unavailable)?;
            record.turn = Some(turn.turn_id);
            record.state = "cancel_requested".into();
        } else {
            record.state = "cancelled".into();
            record.result =
                "Cancelled before worker admission; no task tools were executed.".into();
        }
        self.store.save(&mut record).await?;
        Ok(record)
    }
    pub(super) fn message_ready(&self, task: Uuid) {
        self.message_notifier(task).notify_one();
    }
    pub(super) fn message_notifier(&self, task: Uuid) -> Arc<tokio::sync::Notify> {
        self.waiters
            .lock()
            .entry(task)
            .or_insert_with(|| Arc::new(tokio::sync::Notify::new()))
            .clone()
    }
    pub(super) async fn wait_for_parent(
        &self,
        scope: &TaskScope,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<serde_json::Value> {
        let task = scope.task.ok_or(TaskError::Forbidden)?;
        let mut record = self.authorized(scope, task).await?;
        if record.state != "cancel_requested" {
            record.state = "waiting_parent".into();
            self.store.save(&mut record).await?;
        }
        let notification = self.message_notifier(task);
        loop {
            self.authorized(scope, task).await?;
            let messages = self
                .store
                .pending_messages(task, "parent")
                .await?
                .into_iter()
                .take(1)
                .collect::<Vec<_>>();
            if !messages.is_empty() {
                let ids: Vec<_> = messages
                    .iter()
                    .filter_map(|m| m["message_id"].as_i64())
                    .collect();
                self.store.consume(task, &ids).await?;
                let mut record = self.store.get(scope.principal, task).await?;
                if record.state == "waiting_parent" {
                    record.state = "running".into();
                    self.store.save(&mut record).await?;
                }
                return Ok(
                    json!({"state":"consumed","messages":messages,"instruction":"Incorporate the response and acknowledge applied message IDs through update_task."}),
                );
            }
            tokio::select! {biased;_=cancellation.cancelled()=>return Err(TaskError::Unavailable),_=notification.notified()=>(),_=tokio::time::sleep(std::time::Duration::from_secs(30))=>()}
        }
    }
    pub fn new(gateway: Weak<Gateway>, journal: Arc<CloudJournal>) -> Arc<Self> {
        Arc::new(Self {
            gateway,
            store: TaskStore {
                pool: journal.identity_pool(),
            },
            journal,
            cursor: Default::default(),
            waiters: Default::default(),
            transcripts: Default::default(),
        })
    }
    pub async fn scope(
        self: &Arc<Self>,
        principal: Uuid,
        session: Uuid,
        route: ChannelRoute,
    ) -> Result<TaskScope> {
        let task = self.store.by_session(principal, session).await?;
        Ok(TaskScope {
            service: Arc::downgrade(self),
            principal,
            session,
            root_session: task.as_ref().map_or(session, |r| r.parent_session),
            role: if task.is_some() {
                AgentRole::Worker
            } else {
                AgentRole::Main
            },
            task: task.map(|r| r.id),
            route,
        })
    }
    pub async fn authorized(&self, scope: &TaskScope, id: Uuid) -> Result<TaskRecord> {
        if !self
            .gateway
            .upgrade()
            .ok_or(TaskError::Unavailable)?
            .worker_scope_valid(scope)
            .await
            .map_err(|_| TaskError::Unavailable)?
        {
            return Err(TaskError::Forbidden);
        }
        let record = self.store.get(scope.principal, id).await?;
        if record.parent_session != scope.root_session || scope.task.is_some_and(|task| task != id)
        {
            return Err(TaskError::Forbidden);
        }
        let mut worker_scope = scope.clone();
        worker_scope.session = record.worker_session;
        if !self
            .gateway
            .upgrade()
            .ok_or(TaskError::Unavailable)?
            .worker_scope_valid(&worker_scope)
            .await
            .map_err(|_| TaskError::Unavailable)?
        {
            return Err(TaskError::Forbidden);
        }
        let epoch = crate::memory::MemoryStore::new(&self.journal, scope.principal, 65536)
            .privacy_epoch()
            .await
            .map_err(|_| TaskError::Unavailable)?;
        let parent = self
            .journal
            .session(scope.principal, record.parent_session)
            .await
            .map_err(|_| TaskError::Forbidden)?;
        if record.privacy_epoch != epoch || record.parent_generation != parent.context_generation {
            return Err(TaskError::Forbidden);
        }
        Ok(record)
    }
    pub async fn delegate(
        &self,
        scope: &TaskScope,
        turn: Uuid,
        key: &str,
        input: Delegate,
    ) -> Result<TaskRecord> {
        if scope.role != AgentRole::Main
            || input.goal.trim().is_empty()
            || input.goal.len() > 16384
            || input.constraints.len() > 16384
            || input.acceptance.len() > 8192
            || input
                .workspace
                .as_ref()
                .is_some_and(|path| path.len() > 4096)
            || serde_json::to_vec(&input)?.len() > 32768
        {
            return Err(TaskError::Invalid);
        }
        use sha2::Digest;
        let fingerprint = format!("{:x}", sha2::Sha256::digest(serde_json::to_vec(&input)?));
        if let Some(existing) = self.store.existing(scope, key).await? {
            if existing.request_fingerprint != fingerprint {
                return Err(TaskError::Conflict);
            }
            return Ok(existing);
        }
        let active: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_tasks WHERE principal=? AND parent_session=? AND json_extract(record_json,'$.state') NOT IN ('completed','failed','cancelled','recovery_required','invalidated')")
            .bind(scope.principal.to_string()).bind(scope.root_session.to_string()).fetch_one(&self.store.pool).await?;
        if active >= 8 {
            return Err(TaskError::Limit);
        }
        let gateway = self.gateway.upgrade().ok_or(TaskError::Unavailable)?;
        let parent = self
            .journal
            .session(scope.principal, scope.session)
            .await
            .map_err(|_| TaskError::Forbidden)?;
        let worker = gateway
            .prepare_worker(scope, &input)
            .await
            .map_err(|_| TaskError::Unavailable)?;
        let privacy_epoch = crate::memory::MemoryStore::new(&self.journal, scope.principal, 65536)
            .privacy_epoch()
            .await
            .map_err(|_| TaskError::Unavailable)?;
        let (reply_to, reply_kind) = gateway.worker_parent_reference(scope.session);
        let record = TaskRecord {
            id: Uuid::new_v4(),
            principal: scope.principal,
            parent_session: scope.session,
            worker_session: worker.session_id,
            parent_turn: turn,
            request_key: key.into(),
            request_fingerprint: fingerprint,
            route: scope.route.clone(),
            goal: input.goal,
            reply_to,
            reply_kind,
            constraints: input.constraints,
            acceptance: input.acceptance,
            state: "queued".into(),
            turn: None,
            revision: 0,
            note: String::new(),
            plan: Vec::new(),
            result: String::new(),
            notified: false,
            privacy_epoch,
            parent_generation: parent.context_generation,
            permission_mode: parent.permission_mode,
            private_memory: gateway.worker_private_route(&scope.route),
            created_at: now(),
        };
        self.store.insert(&record).await?;
        Ok(record)
    }
    pub async fn list(&self, scope: &TaskScope) -> Result<serde_json::Value> {
        let records = self.store.list(scope).await?;
        let mut summaries = Vec::new();
        for record in records.into_iter().take(12) {
            if let Ok(record) = self.authorized(scope, record.id).await {
                summaries.push(self.summary(&record));
            }
        }
        Ok(json!({"tasks":summaries}))
    }
    pub fn summary(&self, record: &TaskRecord) -> serde_json::Value {
        let plan: Vec<_> = record
            .plan
            .iter()
            .take(16)
            .map(|item| json!({"name":clip(&item.name,100),"state":item.state}))
            .collect();
        json!({"task_id":record.id,"worker_session":record.worker_session,"state":record.state,"revision":record.revision,"goal":clip(&record.goal,400),"note":clip(&record.note,500),"work_plan":plan,"result":clip(&record.result,1000),"worker_turn":record.turn,"preview":true})
    }
    pub(crate) fn attach_transcript(
        &self,
        scope: &TaskScope,
        transcript: &Arc<parking_lot::RwLock<peri_agent::session::MessageTranscript>>,
    ) {
        if let Some(task) = scope.task {
            let mut live = self.transcripts.write();
            live.retain(|_, transcript| transcript.strong_count() > 0);
            live.insert(task, Arc::downgrade(transcript));
        }
    }
    pub(super) async fn conversation(
        &self,
        record: &TaskRecord,
        after: i64,
        limit: usize,
    ) -> Result<serde_json::Value> {
        let live = self
            .transcripts
            .read()
            .get(&record.id)
            .and_then(Weak::upgrade);
        let (history, flags, source) = if let Some(live) = live {
            let transcript = live.read();
            let history = transcript.canonical_payloads().to_vec();
            let flags = history
                .iter()
                .map(|p| (p.id(), transcript.flags(p.id())))
                .collect::<std::collections::HashMap<_, _>>();
            (history, flags, "live")
        } else {
            let state = self
                .journal
                .session(record.principal, record.worker_session)
                .await
                .map_err(|_| TaskError::Forbidden)?;
            (state.history, state.history_flags, "settled")
        };
        let entries:Vec<_> = history.iter().filter(|p|!flags.get(&p.id()).is_some_and(|f|f.excluded||f.truncated)).filter_map(|p|p.as_message()).enumerate().skip(after.max(0) as usize).take(limit.clamp(1,16)).map(|(i,m)| {
            use peri_acp_types::messages::BaseMessage;
            let role = match m {BaseMessage::Human{..}=>"user",BaseMessage::Ai{..}=>"assistant",BaseMessage::Tool{..}=>"tool",BaseMessage::System{..}=>"system"};
            let text = m.content();
            json!({"cursor":i+1,"message_id":m.id(),"role":role,"text":clip(&text,4000),"content_bytes":text.len(),"truncated":text.chars().count()>4000})
        }).collect();
        Ok(
            json!({"entries":entries,"next_cursor":entries.last().and_then(|e|e["cursor"].as_i64()).unwrap_or(after),"source":source,"exclusions_respected":true}),
        )
    }
    pub async fn tick(&self) -> Result<()> {
        self.waiters
            .lock()
            .retain(|_, waiter| Arc::strong_count(waiter) > 1);
        self.transcripts
            .write()
            .retain(|_, transcript| transcript.strong_count() > 0);
        let Some(gateway) = self.gateway.upgrade() else {
            return Ok(());
        };
        let mut pending = self
            .store
            .unfinished(self.cursor.load(std::sync::atomic::Ordering::Relaxed))
            .await?;
        if pending.is_empty() {
            self.cursor.store(0, std::sync::atomic::Ordering::Relaxed);
            pending = self.store.unfinished(0).await?;
        }
        for (cursor, mut record) in pending {
            // Unreachable delivery targets must not starve newer tasks. This
            // cursor only rotates observation; it never replays an admission.
            self.cursor
                .store(cursor, std::sync::atomic::Ordering::Relaxed);
            let epoch = crate::memory::MemoryStore::new(&self.journal, record.principal, 65536)
                .privacy_epoch()
                .await
                .map_err(|_| TaskError::Unavailable)?;
            let parent = self
                .journal
                .session(record.principal, record.parent_session)
                .await
                .map_err(|_| TaskError::Forbidden)?;
            let scope = self.scope_record(&record);
            let device_valid = gateway
                .worker_scope_valid(&scope)
                .await
                .map_err(|_| TaskError::Unavailable)?;
            if epoch != record.privacy_epoch
                || parent.context_generation != record.parent_generation
                || !device_valid
            {
                if let Some(turn) = record.turn {
                    let _ = gateway.cancel_worker(&record, turn).await;
                }
                record.state = "invalidated".into();
                record.notified = true;
                self.release_settled(record.principal, record.worker_session)
                    .await?;
                match self.store.save(&mut record).await {
                    Ok(()) | Err(TaskError::Conflict) => (),
                    Err(e) => return Err(e),
                }
                continue;
            }
            if record.turn.is_none() && !record.terminal() {
                match gateway.start_worker(&record).await {
                    Ok(turn) => {
                        record = self.store.get(record.principal, record.id).await?;
                        record.turn = Some(turn.turn_id);
                        if record.state == "queued" {
                            record.state = "running".into();
                        }
                        match self.store.save(&mut record).await {
                            Ok(()) | Err(TaskError::Conflict) => (),
                            Err(e) => return Err(e),
                        }
                    }
                    Err(
                        crate::gateway::GatewayError::Busy
                        | crate::gateway::GatewayError::Connection
                        | crate::gateway::GatewayError::NotConfigured
                        | crate::gateway::GatewayError::Runtime(crate::RuntimeError::State(
                            crate::state::StateError::Limit | crate::state::StateError::Busy,
                        )),
                    ) => (),
                    Err(_) => { /* Retain the original identity; query the same admission next tick. */
                    }
                }
                continue;
            }
            if record.turn.is_none() && record.terminal() {
                self.release_settled(record.principal, record.worker_session)
                    .await?;
                if gateway.notify_worker_result(&record).await.is_ok() {
                    record.notified = true;
                }
                match self.store.save(&mut record).await {
                    Ok(()) | Err(TaskError::Conflict) => (),
                    Err(e) => return Err(e),
                }
                continue;
            }
            if let Some(turn) = record.turn {
                let turn = self
                    .journal
                    .turn(record.principal, record.worker_session, turn)
                    .await
                    .map_err(|_| TaskError::Unavailable)?;
                let state = match turn.state {
                    TurnState::Completed => "completed",
                    TurnState::Failed => "failed",
                    TurnState::Interrupted => "cancelled",
                    TurnState::RecoveryRequired => "recovery_required",
                    _ => continue,
                };
                let worker = self
                    .journal
                    .session(record.principal, record.worker_session)
                    .await
                    .map_err(|_| TaskError::Unavailable)?;
                record.state = if worker.execution_settled() {
                    state.into()
                } else {
                    "recovery_required".into()
                };
                self.release_settled(record.principal, record.worker_session)
                    .await?;
                record.result = clip(
                    &turn
                        .replies
                        .iter()
                        .map(|r| r.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                    12000,
                );
                if record.state == "completed" && record.result.is_empty() {
                    record.result="Execution loop ended without a textual handoff. Inspect retained evidence; completion has not been independently verified.".into();
                }
                match self.store.save(&mut record).await {
                    Ok(()) => (),
                    Err(TaskError::Conflict) => continue,
                    Err(e) => return Err(e),
                }
                if gateway.notify_worker_result(&record).await.is_ok() {
                    record.notified = true;
                }
                match self.store.save(&mut record).await {
                    Ok(()) | Err(TaskError::Conflict) => (),
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(())
    }
    fn scope_record(&self, record: &TaskRecord) -> TaskScope {
        TaskScope {
            service: Weak::new(),
            principal: record.principal,
            session: record.worker_session,
            root_session: record.parent_session,
            task: Some(record.id),
            role: AgentRole::Worker,
            route: record.route.clone(),
        }
    }
    pub async fn observe(
        &self,
        scope: &TaskScope,
        event: &peri_acp_types::event_v2::RenderEvent,
    ) -> Result<()> {
        let Some(task) = scope.task else {
            return Ok(());
        };
        use peri_acp_types::event_v2::RenderEvent;
        let preview = match event {
            RenderEvent::ToolStarted {
                tool_call_id,
                name,
                input,
                ..
            } => {
                json!({"kind":"tool_started","call_id":tool_call_id,"name":name,"input_preview":clip(&input.to_string(),2000),"observed_at":now()})
            }
            RenderEvent::ToolEnded {
                tool_call_id,
                name,
                output,
                is_error,
                ..
            } => {
                json!({"kind":"tool_finished","call_id":tool_call_id,"name":name,"output_preview":clip(output,1800),"output_bytes":output.len(),"truncated":output.chars().count()>1800,"is_error":is_error,"observed_at":now()})
            }
            RenderEvent::TurnCompleted {
                finalized_messages, ..
            } => {
                // Persist finalized visible text once per iteration, not one
                // full-sync transaction per streaming token. Never read thinking.
                let text = finalized_messages
                    .iter()
                    .filter(|m| matches!(m, peri_acp_types::messages::BaseMessage::Ai { .. }))
                    .map(|m| m.content())
                    .collect::<Vec<_>>()
                    .join("\n");
                if text.trim().is_empty() {
                    return Ok(());
                }
                json!({"kind":"visible_commentary","text":clip(&text,2000),"text_bytes":text.len(),"truncated":text.chars().count()>2000,"observed_at":now()})
            }
            RenderEvent::HitlPending {
                tool_call_id,
                tool_name,
                ..
            } => {
                json!({"kind":"waiting_for_approval","call_id":tool_call_id,"name":tool_name,"observed_at":now()})
            }
            _ => return Ok(()),
        };
        self.store.append_trace(task, preview).await
    }
}
