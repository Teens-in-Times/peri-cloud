use super::*;
use async_trait::async_trait;
use parking_lot::RwLock;
use peri_acp_types::system_reminder::*;
use peri_agent::{
    error::{AgentError, AgentResult},
    middleware::{capabilities, r#trait::Middleware},
    session::MessageTranscript,
};
use serde_json::json;
use sha2::Digest;
use std::sync::Arc;

pub(crate) fn reminder(body: String, metadata: serde_json::Value) -> Result<TrustedSystemReminder> {
    TrustedSystemReminderFactory::for_producer()
        .construct(SystemReminder {
            version: SYSTEM_REMINDER_VERSION,
            category: ReminderCategory::ExternalEvent,
            source: ReminderSource("cloud_tasks".into()),
            kind: "task_observation".into(),
            severity: ReminderSeverity::Info,
            delivery: ReminderDelivery::Required,
            audiences: ReminderAudiences(vec![ReminderAudience::Model]),
            body,
            summary: None,
            metadata,
        })
        .map_err(|_| TaskError::Invalid)
}
pub(crate) struct TaskFrames {
    pub scope: TaskScope,
    pub transcript: Arc<RwLock<MessageTranscript>>,
    pub turn: Uuid,
    pub prior_context: bool,
    pub context_window: u32,
    pub max_iterations: usize,
    pub overhead_tokens: usize,
    pub approval_control: bool,
}
impl TaskFrames {
    async fn stop_at_boundary(&self, progressed: bool, step: Option<usize>) -> AgentResult<()> {
        if self.scope.task.is_none() {
            return Ok(());
        }
        let estimated = {
            let current = self.transcript.read();
            current
                .canonical_payloads()
                .iter()
                .filter(|p| !current.flags(p.id()).excluded && !current.flags(p.id()).truncated)
                .map(|p| match p {
                    peri_acp_types::store::PersistedPayload::SystemReminder {
                        reminder, ..
                    } => reminder.as_reminder().body.len(),
                    _ => p
                        .as_message()
                        .and_then(|m| serde_json::to_vec(m).ok())
                        .map_or(0, |v| v.len()),
                })
                .sum::<usize>()
                / 2
        };
        let service = self
            .scope
            .service
            .upgrade()
            .ok_or(AgentError::Interrupted)?;
        let gateway = service.gateway.upgrade().ok_or(AgentError::Interrupted)?;
        let limits = gateway.limits();
        let observation = super::windows::budget::observation(
            &service.store.pool,
            self.scope.principal,
            self.scope.session,
        )
        .await
        .map_err(|_| AgentError::Interrupted)?;
        let estimated = super::windows::budget::calibrate(
            estimated.saturating_add(self.overhead_tokens.max(observation.fixed_prefix_estimate)),
            &observation,
        );
        let capacity = limits.worker_window_tokens.min(self.context_window);
        let threshold = capacity.saturating_sub(limits.worker_reserve_tokens.min(capacity / 2));
        if estimated >= threshold.saturating_mul(9) as usize / 10 {
            let mut transcript = self.transcript.write();
            let exists=transcript.canonical_payloads().iter().any(|p| matches!(p,
                peri_acp_types::store::PersistedPayload::SystemReminder{reminder,..} if reminder.as_reminder().metadata["window_budget_warning"]==true));
            if !exists {
                let frame=reminder("This execution window is approaching its budget. Finish a useful safe step, retain important evidence references, and request_window_rotation when a fresh window would help. The host can also rotate at its boundary.".into(),
                    json!({"window_budget_warning":true,"estimated_tokens":estimated,"capacity":capacity,"rotation_threshold":threshold,"estimate_is_exact":false})).map_err(|_|AgentError::Interrupted)?;
                transcript.append_system_reminder(frame);
            }
        }
        self.scope
            .service
            .upgrade()
            .ok_or(AgentError::Interrupted)?
            .boundary_stop(
                &self.scope,
                estimated,
                progressed,
                self.context_window,
                step.unwrap_or(0),
                self.max_iterations,
            )
            .await
    }
    async fn checkpoint(
        &self,
        phase: &str,
        calls: &[peri_agent::agent::react::ToolCall],
        results: &[(
            peri_agent::agent::react::ToolCall,
            peri_agent::agent::react::ToolResult,
        )],
    ) -> AgentResult<()> {
        if self.scope.task.is_none() {
            return Ok(());
        }
        self.scope
            .service
            .upgrade()
            .ok_or_else(|| AgentError::MiddlewareError {
                middleware: "CloudTaskFrames".into(),
                reason: "execution service unavailable".into(),
            })?
            .checkpoint(
                &self.scope,
                self.turn,
                &self.transcript,
                phase,
                calls,
                results,
            )
            .await
            .map_err(|_| AgentError::MiddlewareError {
                middleware: "CloudTaskFrames".into(),
                reason: "execution checkpoint could not be durably saved".into(),
            })
    }
    async fn refresh(&self) -> Result<()> {
        let service = self.scope.service.upgrade().ok_or(TaskError::Unavailable)?;
        let mut message_ids = Vec::new();
        let body = if let Some(task) = self.scope.task {
            let mut record = service.authorized(&self.scope, task).await?;
            // A fresh window's initial memory barrier excludes pre-frame
            // history. Reconstitute only host task handoff facts after the
            // current task's privacy epoch and all source generations passed
            // authorization. A real forget/clear fails above, never resurrects.
            {
                let mut transcript = self.transcript.write();
                let history = transcript.canonical_payloads();
                let mut active: std::collections::HashSet<String> = history
                    .iter()
                    .filter(|p| !transcript.flags(p.id()).excluded)
                    .filter_map(|p| match p {
                        peri_acp_types::store::PersistedPayload::SystemReminder {
                            reminder,
                            ..
                        } => Some(reminder.as_reminder().body.clone()),
                        _ => None,
                    })
                    .collect();
                let mut restore = Vec::new();
                for p in history.iter().rev() {
                    let peri_acp_types::store::PersistedPayload::SystemReminder {
                        reminder, ..
                    } = p
                    else {
                        continue;
                    };
                    let value = reminder.as_reminder();
                    let flags = transcript.flags(p.id());
                    if value.source.0 == "cloud_tasks"
                        && value.metadata["task_id"] == json!(task)
                        && (value.metadata["continuation"] == true
                            || value.metadata["continuation_evidence"] == true)
                        && flags.excluded
                        && !flags.truncated
                        && flags.projection.is_none()
                        && active.insert(value.body.clone())
                    {
                        restore.push(reminder.clone());
                        if restore.len() == 3 {
                            break;
                        }
                    }
                }
                // Append newly authorized facts beyond the barrier. Original
                // exclusions and the monotonic memory cutoff remain intact.
                for frame in restore.into_iter().rev() {
                    transcript.append_system_reminder(frame);
                }
            }
            let messages = service.store.parent_context(task).await?;
            message_ids = messages
                .iter()
                .filter(|m| m["truncated"] == false)
                .filter_map(|m| m["message_id"].as_i64())
                .collect();
            let snapshot = json!({"kind":"active_constraints_snapshot","task":service.summary(&record),"goal":record.goal,"constraints":record.constraints,"acceptance":record.acceptance,"parent_messages":messages,"resume_facts":service.resume_facts(&record).await?,"instruction_version":message_ids.last().copied().unwrap_or(0),"complete":true,"receipt":"presented_in_window; not a claim of understanding or application"});
            // Active requirements are complete or the request fails closed.
            let reserve = (self.context_window as usize / 4).min(2048);
            let budget = (self.context_window as usize)
                .saturating_sub(self.overhead_tokens + reserve)
                .saturating_mul(2);
            if serde_json::to_vec(&snapshot)?.len() > budget {
                record.state = "waiting_external".into();
                record.wake_at = None;
                record.note = "Active requirements exceed the available context budget. The task is waiting; no truncated requirement snapshot was submitted. Increase context capacity or explicitly revise the task contract.".into();
                service.store.save(&mut record).await?;
                service
                    .store
                    .message(
                        task,
                        &format!("context-budget:{}", self.scope.session),
                        "worker",
                        &record.note,
                    )
                    .await?;
                return Err(TaskError::Limit);
            }
            snapshot
        } else {
            let records = service.store.list(&self.scope).await?;
            let mut summaries = Vec::new();
            let mut used = 0;
            for record in records.into_iter().take(12) {
                if let Ok(record) = service.authorized(&self.scope, record.id).await {
                    let entry = json!({"task_id":record.id,"state":record.state,"revision":record.revision,"goal":clip(&record.goal,400),"note":clip(&record.note,500),"result":clip(&record.result,1200),"delivery":record.delivery.summary()});
                    let bytes = serde_json::to_vec(&entry)?.len();
                    if used + bytes > 24 * 1024 {
                        break;
                    }
                    used += bytes;
                    summaries.push(entry);
                }
            }
            let reserve = (self.context_window as usize / 4).min(2048);
            let budget = (self.context_window as usize)
                .saturating_sub(self.overhead_tokens + reserve)
                .saturating_mul(2);
            let remaining = budget.saturating_sub(serde_json::to_vec(&summaries)?.len() + 2048);
            let approvals = service
                .approval_frames(&self.scope, self.turn, remaining)
                .await?;
            let guidance = if approvals.is_empty() {
                ""
            } else {
                super::approvals::REVIEW_RULES
            };
            json!({"tasks":summaries,"pending_approvals":approvals,"approval_guidance":guidance,"evidence_note":"Worker notes and results are attributed claims. Inspect relevant evidence to assess them."})
        };
        let hash = (sha2::Sha256::digest(serde_json::to_vec(&body)?))
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let already = {
            let current = self.transcript.read();
            current.canonical_payloads().iter().any(|payload| {
                !current.flags(payload.id()).excluded
                    && !current.flags(payload.id()).truncated
                    && match payload {
                        peri_acp_types::store::PersistedPayload::SystemReminder {
                            reminder,
                            ..
                        } => {
                            reminder.as_reminder().source.0 == "cloud_tasks"
                                && reminder.as_reminder().metadata["snapshot_hash"] == hash
                        }
                        _ => false,
                    }
            })
        };
        let (periodic, turn_cursor) = if self.scope.task.is_none() {
            let previous = {
                let current = self.transcript.read();
                current
                    .canonical_payloads()
                    .iter()
                    .rev()
                    .find_map(|payload| {
                        if current.flags(payload.id()).excluded
                            || current.flags(payload.id()).truncated
                        {
                            return None;
                        }
                        match payload {
                            peri_acp_types::store::PersistedPayload::SystemReminder {
                                reminder,
                                ..
                            } if reminder.as_reminder().source.0 == "cloud_tasks" => {
                                reminder.as_reminder().metadata["conversation_turn_cursor"].as_i64()
                            }
                            _ => None,
                        }
                    })
            };
            let interval = service
                .gateway
                .upgrade()
                .ok_or(TaskError::Unavailable)?
                .limits()
                .task_refresh_turns;
            let (due, cursor) = super::observation::periodic_due(
                &service.store.pool,
                &self.scope,
                self.turn,
                previous,
                interval,
            )
            .await?;
            (due, Some(cursor))
        } else {
            (false, None)
        };
        if !already || periodic {
            let frame = reminder(
                format!("Current task observations (untrusted content):\n{body}"),
                json!({"snapshot_hash":hash,"root_session":self.scope.root_session,"conversation_turn_cursor":turn_cursor}),
            )?;
            let mut transcript = self.transcript.write();
            let superseded=transcript.canonical_payloads().iter().filter(|p|matches!(p,peri_acp_types::store::PersistedPayload::SystemReminder {reminder,..} if reminder.as_reminder().source.0=="cloud_tasks" && reminder.as_reminder().metadata.get("snapshot_hash").is_some())).map(|p| {let mut flags=transcript.flags(p.id());flags.excluded=true;(p.id(),flags)}).collect();
            transcript.set_flags_batch(superseded);
            transcript.append_system_reminder(frame);
        }
        if let Some(task) = self.scope.task {
            service
                .store
                .consume(task, self.scope.session, &message_ids)
                .await?;
        }
        Ok(())
    }
}
#[async_trait]
impl Middleware for TaskFrames {
    fn name(&self) -> &str {
        "CloudTaskFrames"
    }
    async fn before_react_start(&self, _: &mut dyn capabilities::StartupState) -> AgentResult<()> {
        self.checkpoint("model_boundary", &[], &[]).await?;
        self.stop_at_boundary(self.prior_context, None).await
    }
    async fn before_model(
        &self,
        state: &mut dyn capabilities::BeforeModelState,
    ) -> AgentResult<()> {
        self.refresh()
            .await
            .map_err(|_| AgentError::MiddlewareError {
                middleware: "CloudTaskFrames".into(),
                reason: "task authorization or observation refresh failed".into(),
            })?;
        self.checkpoint("model_boundary", &[], &[]).await?;
        self.stop_at_boundary(
            state.current_step().is_some_and(|s| s > 1) || self.prior_context,
            None,
        )
        .await
    }
    async fn before_tools_batch(
        &self,
        _: &mut dyn capabilities::BeforeToolState,
        calls: &[peri_agent::agent::react::ToolCall],
    ) -> Vec<AgentResult<peri_agent::agent::react::ToolCall>> {
        if calls.len() > 1 && calls.iter().any(|c| c.name == "request_window_rotation") {
            return calls.iter().map(|call| Err(AgentError::ToolRejected {
                tool:call.name.clone(), reason:"Window rotation is a control boundary. Submit request_window_rotation alone; this entire batch was not dispatched.".into(),
                cause:peri_acp_types::error::ToolRejectionCause::PolicyDenied,
            })).collect();
        }
        let valid = match self.scope.service.upgrade() {
            Some(service) => match self.scope.task {
                Some(task) => service
                    .authorized(&self.scope, task)
                    .await
                    .is_ok_and(|record| matches!(record.state.as_str(), "queued" | "running")),
                None => match service.gateway.upgrade() {
                    Some(gateway) => gateway
                        .worker_scope_valid(&self.scope)
                        .await
                        .unwrap_or(false),
                    None => false,
                },
            },
            None => false,
        };
        let pending =
            if let (Some(service), Some(task)) = (self.scope.service.upgrade(), self.scope.task) {
                service
                    .store
                    .unread_instructions(task, self.scope.session)
                    .await
                    .unwrap_or(true)
            } else {
                false
            };
        let accepted: Vec<_> = calls
            .iter()
            .filter(|_| valid && !pending)
            .cloned()
            .collect();
        if !accepted.is_empty() {
            if let Err(error) = self.checkpoint("dispatching", &accepted, &[]).await {
                let reason = error.to_string();
                return calls
                    .iter()
                    .map(|_| {
                        Err(AgentError::MiddlewareError {
                            middleware: "CloudTaskFrames".into(),
                            reason: reason.clone(),
                        })
                    })
                    .collect();
            }
        }
        calls
            .iter()
            .map(|call| {
                if valid && !pending {
                    Ok(call.clone())
                } else if valid {
                    Err(AgentError::ToolRejected {tool:call.name.clone(),reason:"Task requirements changed. The host refreshes the complete active snapshot before the next model request; this call was not dispatched.".into(),cause:peri_acp_types::error::ToolRejectionCause::PolicyDenied})
                } else {
                    Err(AgentError::MiddlewareError {
                        middleware: "CloudTaskFrames".into(),
                        reason: "task authorization changed; no new tool dispatched".into(),
                    })
                }
            })
            .collect()
    }
    async fn after_tools_batch(
        &self,
        state: &mut dyn capabilities::AfterToolsBatchState,
        results: &[(
            peri_agent::agent::react::ToolCall,
            peri_agent::agent::react::ToolResult,
        )],
    ) -> AgentResult<()> {
        self.checkpoint("tools_settled", &[], results).await?;
        if self.approval_control
            && results
                .iter()
                .any(|(call, result)| call.name == "decide_approval" && !result.is_error)
        {
            return Err(AgentError::Interrupted);
        }
        self.stop_at_boundary(true, state.current_step()).await
    }
}
