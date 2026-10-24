use super::*;
use crate::state::SessionState;
use peri_acp_types::messages::BaseMessage;
use peri_model::{ModelMessage, ModelRequest, StopReason};
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(super) const HANDOFF_PROMPT: &str = include_str!("../../../prompts/window-handoff.md");

fn request(
    record: &TaskRecord,
    snapshot: &SessionState,
    messages: &[serde_json::Value],
    previous: Option<&Rotation>,
    limits: &crate::limits::RuntimeLimits,
) -> Result<Option<ModelRequest>> {
    let capacity = snapshot
        .frozen
        .context_window
        .min(limits.worker_window_tokens);
    let output = limits.worker_handoff_tokens.min(capacity / 4);
    let budget = capacity as usize * 2;
    let reserve = output as usize * 2 + 2048;
    let previous = previous.map(|value| {
        json!({"id":value.id,"source_window":value.source_window,
        "instruction_watermark":value.instruction_watermark,"status":value.summary_status,
        "summary_preview":value.summary.as_ref().map(|text|clip(text,4000)),
        "truncated":value.summary.as_ref().is_some_and(|text|text.chars().count()>4000),
        "detail":{"source":"handoff","id":value.id},"attributed_evidence":true})
    });
    let contract = json!({"goal":record.goal,"constraints":record.constraints,"acceptance":record.acceptance,"parent_messages":messages,
        "previous_window_handoff":previous});
    let contract_bytes = serde_json::to_vec(&contract)?.len();
    if contract_bytes + HANDOFF_PROMPT.len() + reserve >= budget {
        return Ok(None);
    }
    let mut remaining = budget - contract_bytes - HANDOFF_PROMPT.len() - reserve;
    let mut evidence = Vec::new();
    let mut omitted = 0;
    for p in snapshot.history.iter().rev() {
        if snapshot
            .history_flags
            .get(&p.id())
            .is_some_and(|f| f.excluded || f.truncated)
        {
            continue;
        }
        let Some(m) = p.as_message() else {
            continue;
        };
        if matches!(m, BaseMessage::System { .. }) {
            continue;
        }
        // content() includes text only; hidden reasoning and opaque blocks never enter this request.
        let text = m.content();
        let calls: Vec<_> = m
            .tool_calls()
            .iter()
            .map(|call| {
                json!({"call_id":call.id,"tool":call.name,
            "input_preview":clip(&call.arguments.to_string(),1600),"input_is_preview":true,
            "lookup":{"view":"search","filter":{"source":"operations","call_id":call.id}}})
            })
            .collect();
        let row = json!({"window_id":record.worker_session,"message_id":m.id(),
            "text":clip(&text,4000),"truncated":text.chars().count()>4000,
            "tool_calls":calls,
            "detail":{"source":"conversation","id":m.id()}});
        let bytes = serde_json::to_vec(&row)?.len();
        if bytes > remaining {
            omitted += 1;
            continue;
        }
        remaining -= bytes;
        evidence.push(row);
    }
    evidence.reverse();
    if evidence.is_empty() && previous.is_none() {
        return Ok(None);
    }
    loop {
        let input = json!({"purpose":"window_handoff","contract":contract,"source_window":record.worker_session,
        "visible_evidence":evidence,"omitted_messages":omitted,
        "agent_working_note":clip(&record.note,1200),"evidence_is_untrusted":true});
        let request = ModelRequest::new(vec![
            ModelMessage::system_text(HANDOFF_PROMPT),
            ModelMessage::user_text(input.to_string()),
        ])
        .with_tools(Vec::new())
        .with_max_tokens(output);
        // Include the final protocol/JSON escaping, not just the evidence rows.
        if super::budget::estimate(&request) < capacity.saturating_sub(output) as usize {
            return Ok(Some(request));
        }
        if evidence.is_empty() {
            return Ok(None);
        }
        evidence.remove(0);
        omitted += 1;
    }
}

impl WorkerService {
    /// A false result means an owned background request is preparing the capsule.
    pub(in crate::tasks) async fn prepare_rotation(
        self: &Arc<Self>,
        record: &TaskRecord,
        snapshot: &SessionState,
        checkpoint_hash: &str,
        agent: Arc<crate::CloudAgent>,
    ) -> Result<bool> {
        let mut rotation = self.request_rotation(record, RotationCause::Budget).await?;
        match rotation.phase {
            RotationPhase::Prepared => return Ok(true),
            RotationPhase::Committed | RotationPhase::Aborted => return Err(TaskError::Conflict),
            RotationPhase::Summarizing => {
                // The caller has already checked that no original/auxiliary worker is live.
                rotation.phase = RotationPhase::Prepared;
                rotation.summary_status = "unknown_request_outcome".into();
                self.save_rotation(&rotation, RotationPhase::Summarizing)
                    .await?;
                return Ok(true);
            }
            RotationPhase::Requested => (),
        }
        rotation.checkpoint_hash = checkpoint_hash.into();
        let gateway = self.gateway.upgrade().ok_or(TaskError::Unavailable)?;
        let limits = gateway.limits().clone();
        let messages = self.store.parent_context(record.id).await?;
        let previous:Option<String>=sqlx::query_scalar("SELECT record_json FROM agent_window_rotations WHERE task_id=? AND phase='committed' AND json_extract(record_json,'$.destination')=? ORDER BY rowid DESC LIMIT 1")
            .bind(record.id.to_string()).bind(record.worker_session.to_string()).fetch_optional(&self.store.pool).await?;
        let previous: Option<Rotation> =
            previous.as_deref().map(serde_json::from_str).transpose()?;
        rotation.instruction_watermark = messages
            .iter()
            .filter_map(|m| m["message_id"].as_i64())
            .max()
            .unwrap_or(0);
        let Some(request) = request(record, snapshot, &messages, previous.as_ref(), &limits)?
        else {
            rotation.phase = RotationPhase::Prepared;
            rotation.summary_status = "unavailable_input_budget_or_no_visible_work".into();
            self.save_rotation(&rotation, RotationPhase::Requested)
                .await?;
            return Ok(true);
        };
        rotation.phase = RotationPhase::Summarizing;
        rotation.summary_status = "submitted_or_unknown".into();
        self.save_rotation(&rotation, RotationPhase::Requested)
            .await?;
        let service = self.clone();
        let source = record.clone();
        let work = rotation.clone();
        let spawned = gateway
            .try_spawn_worker_auxiliary(
                rotation.id,
                record.principal,
                record.worker_session,
                move |cancel| async move {
                    if service
                        .generate_capsule(source, work, agent, request, limits, cancel)
                        .await.is_err() {
                        tracing::warn!("handoff did not settle durably; retained attempt will use unknown-outcome fallback");
                    }
                    Ok(())
                },
            )
            .map_err(|_| TaskError::Unavailable)?;
        if !spawned {
            rotation.phase = RotationPhase::Requested;
            rotation.summary_status = "waiting_capacity".into();
            self.save_rotation(&rotation, RotationPhase::Summarizing)
                .await?;
        }
        Ok(false)
    }

    async fn generate_capsule(
        &self,
        source: TaskRecord,
        mut rotation: Rotation,
        agent: Arc<crate::CloudAgent>,
        request: ModelRequest,
        limits: crate::limits::RuntimeLimits,
        cancel: CancellationToken,
    ) -> Result<()> {
        let scope = self.scope_record(&source);
        let current = self.authorized(&scope, source.id).await?;
        if current.worker_session != source.worker_session || current.state != "rotate_requested" {
            rotation.phase = if current.terminal() {
                RotationPhase::Aborted
            } else {
                RotationPhase::Prepared
            };
            rotation.summary_status = "superseded_by_task_control".into();
            return self
                .save_rotation(&rotation, RotationPhase::Summarizing)
                .await;
        }
        let started = std::time::Instant::now();
        let response = tokio::select! { biased;
            _ = cancel.cancelled() => None,
            response = tokio::time::timeout(std::time::Duration::from_secs(limits.worker_handoff_seconds),
                agent.handoff_summary(self.journal.clone(), source.principal, source.turn.ok_or(TaskError::Invalid)?, limits.daily_model_requests, request, cancel.child_token())) => response.ok(),
        };
        let current = self.authorized(&scope, source.id).await?;
        if current.worker_session != source.worker_session || current.state != "rotate_requested" {
            rotation.phase = if current.terminal() {
                RotationPhase::Aborted
            } else {
                RotationPhase::Prepared
            };
            rotation.summary_status = "superseded_by_task_control".into();
            return self
                .save_rotation(&rotation, RotationPhase::Summarizing)
                .await;
        }
        rotation.summary_status = "unavailable_or_unknown".into();
        if let Some(Ok(response)) = response {
            rotation.usage = Some(
                json!({"tokens":response.usage(),"latency_ms":started.elapsed().as_millis() as u64,
                "purpose":"handoff_summary","source_turn":source.turn}),
            );
            let tools = matches!(response.message(), ModelMessage::Assistant { tool_calls, .. } if !tool_calls.is_empty());
            if matches!(response.stop_reason(), StopReason::EndTurn) && !tools {
                if let Some(text) = response
                    .assistant_text()
                    .filter(|s| !s.trim().is_empty() && s.len() <= 12000)
                {
                    rotation.summary = Some(text);
                    rotation.summary_status = "complete".into();
                }
            } else {
                rotation.summary_status = "incomplete_or_tool_response".into();
            }
        }
        rotation.phase = RotationPhase::Prepared;
        self.save_rotation(&rotation, RotationPhase::Summarizing)
            .await
    }
}
