//! Persist model-consumption receipts independently from task completion and chat delivery.
use peri_acp_types::{messages::MessageId, store::PersistedPayload, system_reminder::*};
use peri_agent::session::{
    async_result_mailbox::AsyncResultMailbox, MessageSource, MessageTranscript,
};
use std::{collections::HashSet, sync::Arc};

pub(crate) struct TurnResults {
    mailbox: Arc<AsyncResultMailbox>,
    candidates: HashSet<MessageId>,
}

fn is_result(reminder: &TrustedSystemReminder) -> bool {
    reminder.as_reminder().source.0 == "cloud_tasks"
        && reminder.as_reminder().metadata["source"] == "execution_agent"
}

impl TurnResults {
    pub(crate) fn restore(
        transcript: &MessageTranscript,
        mailbox: Arc<AsyncResultMailbox>,
    ) -> Result<Self, String> {
        let history = transcript.canonical_payloads();
        let covered: HashSet<MessageId> = history
            .iter()
            .filter_map(|p| match p {
                PersistedPayload::SystemReminder { reminder, .. }
                    if reminder.as_reminder().source.0 == "cloud_task_processing" =>
                {
                    reminder
                        .as_reminder()
                        .metadata
                        .get("processed_ids")
                        .cloned()
                        .and_then(|v| serde_json::from_value::<Vec<MessageId>>(v).ok())
                }
                _ => None,
            })
            .flatten()
            .collect();
        let mut candidates = HashSet::new();
        for payload in &history {
            if transcript.flags(payload.id()).excluded || covered.contains(&payload.id()) {
                continue;
            }
            if let PersistedPayload::SystemReminder { id, reminder } = payload {
                if is_result(reminder) {
                    mailbox.mark_canonical_ready(
                        *id,
                        reminder,
                        MessageSource::SubAgentComplete,
                        None,
                    )?;
                    candidates.insert(*id);
                }
            }
        }
        Ok(Self {
            mailbox,
            candidates,
        })
    }

    pub(crate) fn expect(&mut self, id: MessageId) {
        self.candidates.insert(id);
    }

    pub(crate) fn settle(self, transcript: &mut MessageTranscript) -> Result<(), String> {
        let pending: HashSet<_> = self
            .mailbox
            .protected_snapshot()
            .ids()
            .into_iter()
            .collect();
        let present: HashSet<_> = transcript
            .canonical_payloads()
            .iter()
            .filter_map(|p| match p {
                PersistedPayload::SystemReminder { id, reminder } if is_result(reminder) => {
                    Some(*id)
                }
                _ => None,
            })
            .collect();
        let mut processed: Vec<_> = self
            .candidates
            .into_iter()
            .filter(|id| present.contains(id) && !pending.contains(id))
            .collect();
        processed.sort_by_key(|id| id.as_uuid());
        if processed.is_empty() {
            return Ok(());
        }
        for batch in processed.chunks(64) {
            let receipt = TrustedSystemReminderFactory::for_producer().construct(SystemReminder {
                version:SYSTEM_REMINDER_VERSION,category:ReminderCategory::Lifecycle,
                source:ReminderSource("cloud_task_processing".into()),kind:"model_consumption_receipt".into(),
                severity:ReminderSeverity::Info,delivery:ReminderDelivery::DiagnosticOnly,
                audiences:ReminderAudiences(vec![ReminderAudience::Diagnostics]),
                body:"Task observations were included in a successfully committed model response.".into(),
                summary:None,metadata:serde_json::json!({"processed_ids":batch}),
            }).map_err(|e| e.to_string())?;
            transcript.append_system_reminder(receipt);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use peri_acp_types::messages::BaseMessage;

    #[test]
    fn failed_consumption_survives_restore_and_success_has_a_separate_durable_receipt() {
        let event = TrustedSystemReminderFactory::for_producer()
            .construct(SystemReminder {
                version: SYSTEM_REMINDER_VERSION,
                category: ReminderCategory::Lifecycle,
                source: ReminderSource("cloud_tasks".into()),
                kind: "task_observation".into(),
                severity: ReminderSeverity::Info,
                delivery: ReminderDelivery::Required,
                audiences: ReminderAudiences(vec![ReminderAudience::Model]),
                body: "worker finished".into(),
                summary: None,
                metadata: serde_json::json!({"source":"execution_agent"}),
            })
            .unwrap();
        let mut transcript = MessageTranscript::new();
        let id = MessageId::new();
        transcript.mirror_committed_reminder(id, event);
        let mailbox = Arc::new(AsyncResultMailbox::new("session", "cloud"));
        let turn = TurnResults::restore(&transcript, mailbox.clone()).unwrap();
        assert_eq!(mailbox.protected_snapshot().ids(), vec![id]);
        let attempt = mailbox
            .capture_request("failed-attempt", "request-1", &[id])
            .unwrap();
        mailbox.block_attempt(&attempt, "model", "failed");
        turn.settle(&mut transcript).unwrap();
        let mut restored =
            MessageTranscript::new().with_own_payloads(transcript.canonical_payloads());
        let next = Arc::new(AsyncResultMailbox::new("session", "cloud"));
        let turn = TurnResults::restore(&restored, next.clone()).unwrap();
        assert_eq!(next.protected_snapshot().ids(), vec![id]);
        let attempt = next
            .capture_request("success-attempt", "request-2", &[id])
            .unwrap();
        let response = BaseMessage::ai("I have checked the result.");
        let response_id = response.id();
        restored.append(response);
        next.cover_after_commit(&attempt, Some(response_id))
            .unwrap();
        turn.settle(&mut restored).unwrap();
        let final_view = MessageTranscript::new().with_own_payloads(restored.canonical_payloads());
        let last = Arc::new(AsyncResultMailbox::new("session", "cloud"));
        TurnResults::restore(&final_view, last.clone()).unwrap();
        assert!(last.protected_snapshot().is_empty());
        assert!(final_view
            .visible_model_messages()
            .unwrap()
            .iter()
            .all(|m| !m.content().contains("processed_ids")));
    }
}
