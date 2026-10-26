//! Read-only conversation intent, compiled from existing authorities, never inferred from trace.
use super::*;
use crate::memory::MemoryStore;
use serde_json::{json, Value};

pub(super) const MAX_BYTES: usize = 12 * 1024;

fn preview(text: &str, chars: usize) -> Value {
    json!({"text":clip(text,chars),"truncated":text.chars().count()>chars})
}
fn page(items: Vec<Value>, limit: usize) -> Value {
    let omitted = items.len().saturating_sub(limit);
    json!({"items":items.into_iter().take(limit).collect::<Vec<_>>(),"omitted":omitted})
}

/// The source itself is bounded, so this is a view, never a complete history assertion.
fn bounded(mut view: Value, budget: usize) -> Result<Value> {
    let budget = budget.min(MAX_BYTES);
    while serde_json::to_vec(&view)?.len() > budget {
        let mut removed = false;
        for key in [
            "recent_outcomes",
            "scheduled_commitments",
            "memory_references",
            "pending_replies",
            "active_tasks",
        ] {
            if view[key]["items"]
                .as_array_mut()
                .is_some_and(|items| items.pop().is_some())
            {
                let omitted = view[key]["omitted"].as_u64().unwrap_or(0);
                view[key]["omitted"] = json!(omitted + 1);
                removed = true;
                break;
            }
        }
        if !removed {
            return Ok(
                json!({"available":false,"reason":"context_budget","read":"inspect_intent"}),
            );
        }
    }
    Ok(view)
}

impl WorkerService {
    pub(super) async fn current_intent(&self, scope: &TaskScope, budget: usize) -> Result<Value> {
        if scope.role != AgentRole::Main
            || scope.task.is_some()
            || scope.session != scope.root_session
        {
            return Err(TaskError::Forbidden);
        }
        let gateway = self.gateway.upgrade().ok_or(TaskError::Unavailable)?;
        if !gateway
            .worker_scope_valid(scope)
            .await
            .map_err(|_| TaskError::Forbidden)?
        {
            return Err(TaskError::Forbidden);
        }
        let private = gateway.worker_private_route(&scope.route);
        if !crate::memory::context::can_attach(
            &self.journal,
            scope.principal,
            scope.session,
            private,
        )
        .await
        .map_err(|_| TaskError::Forbidden)?
        {
            return Err(TaskError::Forbidden);
        }
        let memory = MemoryStore::new(
            &self.journal,
            scope.principal,
            gateway.limits().max_memory_bytes,
        );
        let epoch = memory
            .privacy_epoch()
            .await
            .map_err(|_| TaskError::Unavailable)?;
        let generation = self
            .journal
            .session(scope.principal, scope.session)
            .await
            .map_err(|_| TaskError::Forbidden)?
            .context_generation;
        let goal: Option<String> = sqlx::query_scalar(
            "SELECT goal_json FROM cloud_goals WHERE principal=? AND session_id=?",
        )
        .bind(scope.principal.to_string())
        .bind(scope.session.to_string())
        .fetch_optional(&self.store.pool)
        .await?;
        let goal = goal
            .map(|raw| serde_json::from_str::<peri_acp_types::goal::ThreadGoal>(&raw))
            .transpose()?;
        let goal = if let Some(goal) = goal {
            use sha2::Digest;
            let version = sha2::Sha256::digest(serde_json::to_vec(&(
                &goal.goal_id,
                &goal.objective,
                goal.status,
                &goal.blocked_reason,
            ))?)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
            Some(
                json!({"source":"cloud_goals","id":goal.goal_id,"version":version,
                "status":goal.status,"objective":preview(&goal.objective,400),"created_at":goal.created_at,
                "blocked_reason":goal.blocked_reason.map(|s|preview(&s,200)),"completion_is_verified":false}),
            )
        } else {
            None
        };
        let mut active = Vec::new();
        let mut outcomes = Vec::new();
        let mut pending = Vec::new();
        let mut sessions = vec![scope.session];
        let mut deadlines = Vec::new();
        let records = self.store.list(scope).await?;
        let task_scan_has_more = records.len() > 16;
        // The admission cap is eight active tasks. Scan recent outcomes too,
        // without authorizing an entire retained history on every model boundary.
        for record in records.into_iter().take(16) {
            let record = match self.authorized(scope, record.id).await {
                Ok(record) => record,
                Err(TaskError::Forbidden) => continue,
                Err(error) => return Err(error),
            };
            if record.private_memory != private || (!private && record.route != scope.route) {
                continue;
            }
            sessions.push(record.worker_session);
            deadlines.extend(record.memory_expires_at);
            let instruction: Option<(i64,String)> = sqlx::query_as("SELECT seq,body FROM agent_task_messages WHERE task_id=? AND direction='parent' ORDER BY seq DESC LIMIT 1")
                .bind(record.id.to_string()).fetch_optional(&self.store.pool).await?;
            let brief = json!({"source":"agent_tasks","id":record.id,"revision":record.revision,
                "state":record.state,"objective":preview(&record.goal,200),"wake_at":record.wake_at,
                "created_at":record.created_at,"expires_at":record.memory_expires_at,
                "latest_instruction":instruction.map(|(seq,body)|json!({"source":"agent_task_messages","id":seq,"body":preview(&body,200)})),
                "delivery":record.delivery.summary(),"read":"inspect_task"});
            if record.terminal() {
                outcomes.push(brief);
            } else {
                active.push(brief);
                if record.state == "waiting_parent" {
                    let question: Option<(i64,String)> = sqlx::query_as("SELECT seq,body FROM agent_task_messages WHERE task_id=? AND direction='worker' ORDER BY seq DESC LIMIT 1")
                        .bind(record.id.to_string()).fetch_optional(&self.store.pool).await?;
                    pending.push(json!({"source":"agent_tasks","task_id":record.id,"revision":record.revision,
                        "state":"waiting_parent","question":question.map(|(seq,body)|json!({"source":"agent_task_messages","id":seq,"body":preview(&body,240)})),
                        "respond":"message_task"}));
                }
            }
        }
        pending.extend(
            gateway
                .intent_questions(scope.principal, &sessions)
                .await
                .map_err(|_| TaskError::Unavailable)?,
        );
        deadlines.extend(
            pending
                .iter()
                .filter_map(|item| item["expires_at"].as_i64()),
        );
        let schedules = crate::scheduler::ScheduleStore::new(&self.journal);
        let mut commitments = Vec::new();
        for plan in schedules
            .list(scope.principal)
            .await
            .map_err(|_| TaskError::Unavailable)?
        {
            if plan.session_id == scope.session {
                let state = if plan.deleted {
                    "deleted"
                } else if !plan.enabled {
                    "paused"
                } else if plan.next_fire.is_none() {
                    "finished"
                } else {
                    "scheduled"
                };
                commitments.push(
                    json!({"source":"scheduler","id":plan.id,"revision":plan.revision,
                    "state":state,"title":preview(&plan.title,128),"next_fire":plan.next_fire,
                    "notification":plan.notification,"notify":plan.notify,"read":"schedule_list"}),
                );
            }
        }
        commitments.sort_by_key(|entry| {
            (
                entry["state"] != "scheduled",
                entry["next_fire"].as_i64().unwrap_or(i64::MAX),
            )
        });
        let (preferences, facts, contact) = if private {
            let preferences = self
                .journal
                .preferences(scope.principal)
                .await
                .map_err(|_| TaskError::Unavailable)?;
            let snapshot = memory
                .snapshot()
                .await
                .map_err(|_| TaskError::Unavailable)?;
            deadlines.extend(
                snapshot
                    .items
                    .iter()
                    .filter_map(|item| item.value.valid_until),
            );
            let facts = snapshot
                .items
                .iter()
                .map(|item| {
                    json!({"source":"memory_items","id":item.id,
                "revision":item.revision,"origin":item.source,"kind":item.value.kind,
                "content":preview(&item.value.content,160),"valid_until":item.value.valid_until,
                "read":"GetMemory","assertion":"recorded_memory"})
                })
                .collect::<Vec<_>>();
            let mut facts = page(facts, 4);
            facts["omitted"] =
                json!(facts["omitted"].as_u64().unwrap_or(0) + snapshot.omitted as u64);
            facts["generation"] = json!(snapshot.generation);
            (
                Some(
                    json!({"source":"user_data","revision":preferences.revision,"instructions":preview(&preferences.instructions,400)}),
                ),
                facts,
                Some(
                    schedules
                        .contact_policy(scope.principal)
                        .await
                        .map_err(|_| TaskError::Unavailable)?,
                ),
            )
        } else {
            (None, page(Vec::new(), 4), None)
        };
        // A forget, clear or rewind during compilation invalidates the whole projection.
        if deadlines.iter().any(|at| *at <= now())
            || !gateway
                .worker_scope_valid(scope)
                .await
                .map_err(|_| TaskError::Forbidden)?
            || epoch
                != memory
                    .privacy_epoch()
                    .await
                    .map_err(|_| TaskError::Unavailable)?
            || generation
                != self
                    .journal
                    .session(scope.principal, scope.session)
                    .await
                    .map_err(|_| TaskError::Forbidden)?
                    .context_generation
        {
            return Err(TaskError::Conflict);
        }
        bounded(
            json!({"schema":1,"available":true,"scope":"current_conversation","session_id":scope.session,
            "context_generation":generation,"privacy_epoch":epoch,"private":private,"goal":goal,
            "task_scan":{"limit":16,"has_more":task_scan_has_more},
            "active_tasks":page(active,8),"recent_outcomes":page(outcomes,4),"pending_replies":page(pending,8),
            "preferences":preferences,"memory_references":facts,"scheduled_commitments":page(commitments,6),
            "contact_policy":contact,"complete_history":false,
            "evidence":"A read-only source projection. User/worker text and recorded memory retain their original attribution; execution completion is separate from acceptance. Unrecorded conversational promises are not inferred."}),
            budget,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escaped_previews_remain_bounded_and_explicit_about_omissions() {
        let mut view = json!({"available":true,"active_tasks":page(vec![preview(&"\u{1}".repeat(1000),400);8],8)});
        for key in [
            "recent_outcomes",
            "scheduled_commitments",
            "memory_references",
            "pending_replies",
        ] {
            view[key] = page(Vec::new(), 8);
        }
        let result = bounded(view, 2048).unwrap();
        assert!(serde_json::to_vec(&result).unwrap().len() <= 2048);
        assert!(result["active_tasks"]["omitted"].as_u64().unwrap() > 0);
        assert_eq!(
            bounded(json!({"goal":"x".repeat(2000)}), 100).unwrap()["available"],
            false
        );
    }
}
