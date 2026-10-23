//! A bounded coordination view, without changing the main conversation's canonical history.
use crate::personal::PersonalScope;
use peri_acp_types::{
    messages::MessageId,
    store::{MessageFlags, PersistedPayload},
};
use peri_agent::session::{transcript::TranscriptEntry, MessageTranscript};
use std::collections::{HashMap, HashSet};

fn memory(entry: &TranscriptEntry) -> bool {
    match entry {
        TranscriptEntry::Reminder { id, reminder } => {
            crate::memory::frames::is_frame(&PersistedPayload::SystemReminder {
                id: *id,
                reminder: reminder.clone(),
            })
        }
        _ => false,
    }
}

fn memory_ids(transcript: &MessageTranscript) -> HashSet<MessageId> {
    transcript
        .entries()
        .iter()
        .filter(|entry| !transcript.is_excluded(entry.id()) && memory(entry))
        .map(|entry| entry.id())
        .collect()
}

fn cutoffs(transcript: &MessageTranscript) -> HashSet<String> {
    transcript
        .entries()
        .iter()
        .filter(|entry| !transcript.is_excluded(entry.id()) && memory(entry))
        .filter_map(|entry| match entry {
            TranscriptEntry::Reminder { reminder, .. } => {
                let metadata = &reminder.as_reminder().metadata;
                Some(
                    serde_json::json!([
                        metadata["privacy_epoch"],
                        metadata["context_generation"],
                        metadata["session_generation"],
                        metadata["barrier"]
                    ])
                    .to_string(),
                )
            }
            _ => None,
        })
        .collect()
}

pub(crate) struct ApprovalView {
    original: HashMap<MessageId, MessageFlags>,
    memory: HashSet<MessageId>,
    cutoffs: HashSet<String>,
    expires_at: Option<i64>,
}
impl ApprovalView {
    pub(crate) fn mask(transcript: &mut MessageTranscript, scope: Option<&PersonalScope>) -> Self {
        let memory = memory_ids(transcript);
        let cutoffs = cutoffs(transcript);
        let original: HashMap<_, _> = transcript
            .entries()
            .iter()
            .map(|entry| (entry.id(), transcript.flags(entry.id())))
            .collect();
        let expires_at = scope
            .and_then(crate::memory::frames::archive_fence)
            .and_then(|(_, _, expiry)| expiry);
        for id in original.keys().filter(|id| !memory.contains(*id)) {
            transcript.set_excluded(*id, true);
        }
        Self {
            original,
            memory,
            cutoffs,
            expires_at,
        }
    }

    pub(crate) fn restore(self, transcript: &mut MessageTranscript) {
        // A memory refresh can establish a real privacy cutoff while the control
        // request runs. Do not undo it when removing the temporary view mask.
        let restore = cutoffs(transcript) == self.cutoffs
            && self
                .expires_at
                .is_none_or(|at| at > chrono::Utc::now().timestamp());
        let coordination: Vec<_> = transcript
            .entries()
            .iter()
            .filter(|entry| !self.original.contains_key(&entry.id()) && !memory(entry))
            .map(|entry| entry.id())
            .collect();
        if restore {
            for id in self.original.keys().filter(|id| !self.memory.contains(*id)) {
                transcript.clear_flags(*id);
            }
            transcript.set_flags_batch(
                self.original
                    .into_iter()
                    .filter(|(id, _)| !self.memory.contains(id))
                    .collect(),
            );
        }
        for id in coordination {
            transcript.set_excluded(id, true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use peri_acp_types::{messages::BaseMessage, system_reminder::*};

    #[test]
    fn control_view_restores_chat_but_cannot_undo_a_concurrent_privacy_refresh() {
        let mut transcript = MessageTranscript::new();
        let input = transcript.append(BaseMessage::human("user request"));
        let view = ApprovalView::mask(&mut transcript, None);
        assert!(transcript.is_excluded(input));
        let review = transcript.append(BaseMessage::ai("internal review"));
        view.restore(&mut transcript);
        assert!(!transcript.is_excluded(input));
        assert!(transcript.is_excluded(review));
        let view = ApprovalView::mask(&mut transcript, None);
        let frame=TrustedSystemReminderFactory::for_producer().construct(SystemReminder {
            version:SYSTEM_REMINDER_VERSION,category:ReminderCategory::Lifecycle,
            source:ReminderSource("cloud_memory".into()),kind:"context_frame".into(),
            severity:ReminderSeverity::Info,delivery:ReminderDelivery::Required,
            audiences:ReminderAudiences(vec![ReminderAudience::Model]),body:"new authorized memory after forget".into(),
            summary:None,metadata:serde_json::json!({"version":2,"hash":"fresh","privacy_epoch":1,"catalog_revision":"fresh","expires_at":null,"context_generation":1,"device_id":null,"barrier":null,"session_generation":0,"model_revision":"test"}),
        }).unwrap();
        let fresh = transcript.append_system_reminder(frame);
        view.restore(&mut transcript);
        assert!(
            transcript.is_excluded(input),
            "temporary approval restoration must not resurrect pre-forget chat"
        );
        assert!(!transcript.is_excluded(fresh));
    }
}
