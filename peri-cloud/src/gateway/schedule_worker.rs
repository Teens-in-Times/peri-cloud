//! Cloud admission/delivery adapter. Time progression lives in peri-scheduler.
use super::*;
use crate::scheduler::{Firing, TargetBinding};
use crate::state::{TurnRecord, TurnState};
use async_trait::async_trait;
use peri_scheduler::{
    Clock, LookupOutcome, Occurrence, Scheduler, SqliteStore, SubmitOutcome, TaskDispatcher,
};
use sqlx::Row;
use std::sync::Weak;

struct CloudClock(Arc<IdentityService>);
impl Clock for CloudClock {
    fn now(&self) -> i64 {
        self.0.now()
    }
}
struct CloudDispatcher(Weak<Gateway>);
pub(crate) fn build(
    owner: Weak<Gateway>,
    identity: Arc<IdentityService>,
    store: Arc<SqliteStore>,
) -> Scheduler {
    Scheduler::new(
        store,
        Arc::new(CloudClock(identity)),
        Arc::new(CloudDispatcher(owner)),
    )
}
#[async_trait]
impl TaskDispatcher for CloudDispatcher {
    async fn submit(&self, event: &Occurrence) -> SubmitOutcome {
        let Some(gateway) = self.0.upgrade() else {
            return SubmitOutcome::Unknown;
        };
        let gate = gateway.account_gate(event.owner.0.to_string());
        let _guard = gate.lock().await;
        let _policy = gateway.access_gate.read().await;
        let store = ScheduleStore::new(gateway.journal());
        let result = async {
            let firing = store.firing(event).await?;
            // The receipt lookup must precede current-session resolution.
            if let Some(turn) = gateway
                .journal()
                .scheduled_receipt(firing.principal, event.id, &firing.fingerprint()?)
                .await?
            {
                return Ok(turn);
            }
            gateway.admit_schedule(&firing).await
        }
        .await;
        match result {
            Ok(turn) => SubmitOutcome::Accepted {
                run_ref: turn.turn_id.to_string(),
            },
            Err(GatewayError::Identity(IdentityError::Unauthorized | IdentityError::Forbidden)) => {
                SubmitOutcome::Rejected {
                    reason: "authorization_revoked".into(),
                }
            }
            Err(GatewayError::StaleInteraction) => SubmitOutcome::Rejected {
                reason: "session_selection_changed".into(),
            },
            Err(GatewayError::Schedule(crate::scheduler::ScheduleError::Invalid(reason))) => {
                SubmitOutcome::Rejected {
                    reason: reason.into(),
                }
            }
            Err(
                GatewayError::Busy
                | GatewayError::State(StateError::Busy)
                | GatewayError::Runtime(crate::RuntimeError::State(StateError::Busy)),
            ) => SubmitOutcome::Deferred {
                until: gateway.identity.now() + 5,
                reason: "session_busy".into(),
            },
            Err(GatewayError::Connection | GatewayError::NotConfigured) => {
                SubmitOutcome::Deferred {
                    until: gateway.identity.now() + 15,
                    reason: "device_unavailable".into(),
                }
            }
            Err(GatewayError::Runtime(crate::RuntimeError::State(StateError::Limit))) => {
                SubmitOutcome::Deferred {
                    until: gateway.identity.now() + 30,
                    reason: "quota_or_concurrency_limit".into(),
                }
            }
            Err(GatewayError::Closing | GatewayError::Runtime(crate::RuntimeError::Closing)) => {
                SubmitOutcome::Deferred {
                    until: gateway.identity.now() + 5,
                    reason: "service_closing".into(),
                }
            }
            Err(_) => SubmitOutcome::Unknown,
        }
    }
    async fn lookup(&self, event: &Occurrence) -> LookupOutcome {
        let Some(gateway) = self.0.upgrade() else {
            return LookupOutcome::Unresolved;
        };
        let store = ScheduleStore::new(gateway.journal());
        let result = async {
            let firing = store.firing(event).await?;
            let receipt = gateway
                .journal()
                .scheduled_receipt(event.owner.0, event.id, &firing.fingerprint()?)
                .await?;
            Ok::<_, GatewayError>((
                receipt,
                firing.schedule.binding == TargetBinding::LegacyFrozenSession,
            ))
        }
        .await;
        match result {
            Ok((Some(turn), _)) => LookupOutcome::Accepted {
                run_ref: turn.turn_id.to_string(),
            },
            Ok((None, false)) if !gateway.runtime.has_active_for(event.owner.0) => {
                LookupOutcome::DefinitelyNotAccepted
            }
            _ => LookupOutcome::Unresolved,
        }
    }
}
impl Gateway {
    pub async fn schedule_tick(self: &Arc<Self>) -> GatewayResult<()> {
        self.reap().await?;
        let (sender, receiver) = oneshot::channel();
        {
            let mut owner = self.owner.lock();
            if owner.closing {
                return Err(GatewayError::Closing);
            }
            if owner.tasks.len() >= 128 {
                return Err(GatewayError::Busy);
            }
            let gateway = self.clone();
            owner.tasks.push(tokio::spawn(async move {
                let result = gateway
                    .scheduler
                    .tick()
                    .await
                    .map(|_| ())
                    .map_err(GatewayError::from);
                let uncertain = result.is_err();
                let _ = sender.send(result);
                if uncertain {
                    Err(GatewayError::RecoveryRequired)
                } else {
                    Ok(())
                }
            }));
        }
        receiver.await.map_err(|_| GatewayError::RecoveryRequired)?
    }
    async fn admit_schedule(&self, firing: &Firing) -> GatewayResult<TurnRecord> {
        if self.owner.lock().closing {
            return Err(GatewayError::Closing);
        }
        if !self.route_allowed(&firing.route).await?
            || self
                .identity
                .resolve_channel(&firing.route.identity)
                .await?
                != firing.principal
        {
            return Err(IdentityError::Forbidden.into());
        }
        let store = ScheduleStore::new(self.journal());
        let contact = store.contact_policy(firing.principal).await?;
        if firing.schedule.mode == ScheduleMode::Conversation
            && !contact.allows(self.identity.now(), firing.schedule.notify)
        {
            return Err(crate::scheduler::ScheduleError::Invalid(
                "proactive_contact_disabled_or_outside_window",
            )
            .into());
        }
        // Foreground work and unknown execution keep their actual Runtime slot.
        if self.runtime.has_active_for(firing.principal) {
            return Err(GatewayError::Busy);
        }
        let selected = self
            .store
            .selection(&firing.route, firing.principal)
            .await?;
        let session = match firing.schedule.binding {
            TargetBinding::Chat => match selected {
                Some(session) => session,
                None => {
                    self.ensure_cloud_conversation(&firing.route, firing.principal)
                        .await?
                }
            },
            TargetBinding::Device | TargetBinding::LegacyFrozenSession => {
                if selected != Some(firing.schedule.session_id) {
                    return Err(GatewayError::StaleInteraction);
                }
                firing.schedule.session_id
            }
        };
        let state = self.journal().session(firing.principal, session).await?;
        if !state.execution_settled() || self.journal().has_unconfirmed_turn(session).await? {
            return Err(GatewayError::Busy);
        }
        let agent = if firing.schedule.mode == ScheduleMode::Task
            || firing.schedule.binding == TargetBinding::LegacyFrozenSession
        {
            let id = firing
                .schedule
                .device_id
                .ok_or(GatewayError::StaleInteraction)?;
            let (_, device) = self
                .identity
                .channel_device(&firing.route.identity, id)
                .await?;
            if state.frozen.device_id != Some(id)
                || state.frozen.workspace() != firing.schedule.workspace
            {
                return Err(GatewayError::StaleInteraction);
            }
            if firing.schedule.mode == ScheduleMode::Task {
                let agent = self
                    .connector
                    .open(firing.principal, &device, Some(&state.frozen))
                    .await?;
                self.attach(&firing.route, firing.principal, &device, agent.clone())
                    .await?;
                agent
            } else {
                let agent = self
                    .connector
                    .open_conversation(firing.principal, &state.frozen)
                    .await?;
                self.attach_conversation(&firing.route, firing.principal, agent.clone())
                    .await?;
                agent
            }
        } else {
            let agent = self
                .connector
                .open_conversation(firing.principal, &state.frozen)
                .await?;
            self.attach_conversation(&firing.route, firing.principal, agent.clone())
                .await?;
            agent
        };
        let agent = agent.for_scheduled();
        let push = self
            .adapters
            .get(&firing.route.identity.adapter_instance_id)
            .is_some_and(|a| a.supports_proactive());
        agent.configure_schedule_notifications(push);
        let origin = self
            .origins
            .read()
            .get(&session)
            .cloned()
            .ok_or(GatewayError::RecoveryRequired)?;
        *origin.write() = Some(if push {
            SessionOrigin::Scheduled(firing.request_key.clone())
        } else {
            SessionOrigin::Browser
        });
        Ok(self
            .runtime
            .submit_scheduled(
                crate::SubmitTurn {
                    principal_id: firing.principal,
                    session_id: session,
                    request_key: firing.request_key.clone(),
                    prompt: scheduled_prompt(firing),
                    max_iterations: self.max_iterations,
                },
                agent.for_memory_visibility(self.private_memory_route(&firing.route)),
                crate::state::ScheduledAdmission {
                    occurrence_id: firing.event.id,
                    request_hash: firing.fingerprint()?,
                    route_json: serde_json::to_string(&firing.route)?,
                    admitted_at: self.identity.now(),
                    expires_at: if firing.schedule.mode == ScheduleMode::Conversation {
                        firing.event.start_deadline
                    } else {
                        self.identity
                            .now()
                            .saturating_add(self.limits().max_turn_seconds as i64)
                            .saturating_add(300)
                    },
                    daily_limit: contact.daily_limit,
                },
            )
            .await?)
    }
    pub(crate) async fn project_schedules(&self) -> GatewayResult<()> {
        let store = ScheduleStore::new(self.journal());
        let rows =
            sqlx::query("SELECT * FROM scheduled_admissions WHERE execution_state='submitted'")
                .fetch_all(&store.pool)
                .await?;
        for row in rows {
            let principal = crate::scheduler::store::parse_uuid(row.get("owner"))?;
            let session = crate::scheduler::store::parse_uuid(row.get("session_id"))?;
            let turn_id = crate::scheduler::store::parse_uuid(row.get("turn_id"))?;
            let event_id = crate::scheduler::store::parse_uuid(row.get("occurrence_id"))?;
            let event = store
                .core
                .occurrence(peri_scheduler::OwnerScope(principal), event_id)
                .await?;
            let firing = store.firing(&event).await?;
            let turn = self.journal().turn(principal, session, turn_id).await?;
            if matches!(turn.state, TurnState::Queued | TurnState::Running) {
                continue;
            }
            let state = match turn.state {
                TurnState::Completed => "completed",
                TurnState::Interrupted => "interrupted",
                TurnState::Failed => "failed",
                _ => "recovery_required",
            };
            let reply = turn.replies.last();
            let silence = reply.is_some_and(|r| r.text.trim() == "[SILENT]");
            let push = self
                .adapters
                .get(&firing.route.identity.adapter_instance_id)
                .is_some_and(|a| a.supports_proactive());
            let recorded_delivery: String = row.get("delivery_state");
            let delivery = if recorded_delivery != "pending" {
                recorded_delivery.as_str()
            } else if silence && firing.schedule.notify == Notify::WhenUseful {
                "suppressed"
            } else if silence || reply.is_none() {
                "empty"
            } else if !push {
                "inbox"
            } else {
                "queued"
            };
            let mut tx = store.pool.begin().await?;
            if delivery == "queued" {
                if let Some(reply) = reply {
                    let mut reply = reply.clone();
                    reply.text = activity::reply_preview(&reply.text, turn_id);
                    crate::gateway::store::enqueue(
                        &mut tx,
                        &format!("{}:reply:{}", firing.request_key, reply.message_id),
                        &firing.route,
                        (&firing.request_key, crate::gateway::ReplyKind::Proactive),
                        DeliveryBody::Reply { reply },
                        Some(principal),
                        if firing.schedule.binding == TargetBinding::Chat {
                            None
                        } else {
                            firing.schedule.device_id
                        },
                    )
                    .await?;
                }
            }
            sqlx::query("UPDATE scheduled_admissions SET execution_state=?,delivery_state=? WHERE owner=? AND occurrence_id=? AND execution_state='submitted'")
                .bind(state).bind(delivery).bind(principal.to_string()).bind(event_id.to_string()).execute(&mut *tx).await?;
            tx.commit().await?;
        }
        Ok(())
    }
    pub(crate) async fn scheduled_delivery_current(&self, key: &str) -> GatewayResult<bool> {
        if !key.starts_with("schedule:") {
            return Ok(true);
        }
        let Some((request, _)) = key.split_once(":reply:") else {
            return Ok(true);
        };
        let store = ScheduleStore::new(self.journal());
        let row = sqlx::query("SELECT * FROM scheduled_admissions WHERE request_key=?")
            .bind(request)
            .fetch_optional(&store.pool)
            .await?;
        let Some(row) = row else {
            return Ok(false);
        };
        let principal = crate::scheduler::store::parse_uuid(row.get("owner"))?;
        let session = crate::scheduler::store::parse_uuid(row.get("session_id"))?;
        let route: ChannelRoute = serde_json::from_str(row.get("route_json"))?;
        let event = store
            .core
            .occurrence(
                peri_scheduler::OwnerScope(principal),
                crate::scheduler::store::parse_uuid(row.get("occurrence_id"))?,
            )
            .await?;
        let firing = store.firing(&event).await?;
        let state = self.journal().session(principal, session).await?;
        if row.get::<String, _>("delivery_state") != "queued"
            || state.context_generation != row.get::<i64, _>("context_generation") as u64
            || self.store.selection(&route, principal).await? != Some(session)
            || self.identity.now() > row.get::<i64, _>("expires_at")
        {
            return Ok(false);
        }
        let contact = store.contact_policy(principal).await?;
        Ok(firing.schedule.mode != ScheduleMode::Conversation
            || contact.allows(self.identity.now(), firing.schedule.notify))
    }
    pub(crate) async fn settle_schedule_deliveries(&self) -> GatewayResult<()> {
        let store = ScheduleStore::new(self.journal());
        let rows:Vec<(String,String)>=sqlx::query_as("SELECT occurrence_id,request_key FROM scheduled_admissions WHERE delivery_state='queued'").fetch_all(&store.pool).await?;
        for (id, key) in rows {
            let sends: Vec<(String,)> =
                sqlx::query_as("SELECT state FROM gateway_outbox WHERE semantic_key LIKE ?")
                    .bind(format!("{key}:reply:%"))
                    .fetch_all(&store.pool)
                    .await?;
            if sends.is_empty()
                || sends
                    .iter()
                    .any(|(s,)| matches!(s.as_str(), "ready" | "sending"))
            {
                continue;
            }
            let state = if sends.iter().any(|(s,)| s == "unconfirmed") {
                "unknown"
            } else if sends.iter().any(|(s,)| s == "rejected") {
                "rejected"
            } else if sends.iter().any(|(s,)| s == "suppressed") {
                "suppressed"
            } else {
                "delivered"
            };
            sqlx::query("UPDATE scheduled_admissions SET delivery_state=? WHERE occurrence_id=? AND delivery_state='queued'").bind(state).bind(id).execute(&store.pool).await?;
        }
        Ok(())
    }
}
fn scheduled_prompt(firing: &Firing) -> MessageContent {
    let behavior=match firing.schedule.mode {
        ScheduleMode::Conversation=>"This is an authorized proactive conversation check-in. Use this conversation's history and account preferences to write a natural, warm, concise message. Do not invent facts about the user's feelings or activity. You have no device tools in this mode. Ask at most one gentle question; the user can reply in this same conversation.",
        ScheduleMode::Task=>"Execute this previously authorized scheduled task now using the normal tool permission policy.",
    };
    let notify = if firing.schedule.notify == Notify::WhenUseful {
        "If this check-in would be unhelpful or repetitive, return exactly [SILENT] instead of a message."
    } else {
        "The user explicitly requested a message at this time. Return the message itself, without scheduler status or delivery instructions."
    };
    MessageContent::Text(format!("[Scheduled event: {} at {} UTC; this is not a new user message.]\n{behavior}\n{notify}\nThe gateway delivers your final response to the task's saved origin. Do not create new schedules or call send commands.\nUser-authorized task:\n{}",firing.schedule.id,firing.event.planned_at,firing.schedule.prompt))
}
