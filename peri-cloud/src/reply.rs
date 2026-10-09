use peri_acp_types::messages::BaseMessage;
use peri_acp_types::store::PersistedPayload;
use serde::{Deserialize, Serialize};

/// Channel-neutral assistant text. No raw event, tool output or thinking field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatReply {
    pub message_id: String,
    pub text: String,
}

pub(crate) fn replies(payloads: &[PersistedPayload]) -> Vec<ChatReply> {
    payloads
        .iter()
        .filter_map(|payload| {
            let PersistedPayload::Message(message @ BaseMessage::Ai { .. }) = payload else {
                return None;
            };
            let text = message.content();
            if text.trim().is_empty() {
                return None;
            }
            Some(ChatReply {
                message_id: message.id().as_uuid().to_string(),
                text,
            })
        })
        .collect()
}

#[cfg(test)]
#[path = "reply_test.rs"]
mod tests;
