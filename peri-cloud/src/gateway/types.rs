use async_trait::async_trait;
use peri_acp_types::interaction::{InteractionContext, QuestionAnswer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{GatewayError, GatewayResult};
use crate::identity::ChannelIdentity;
use crate::ChatReply;

/// Private per-sender session even inside a group conversation. A transport
/// creates this from authenticated platform events, never from model arguments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelRoute {
    pub identity: ChannelIdentity,
    pub conversation_id: String,
}

impl ChannelRoute {
    pub(crate) fn key(&self) -> GatewayResult<String> {
        for (value, limit) in [
            (&self.identity.adapter_instance_id, 128),
            (&self.identity.external_user_id, 256),
            (&self.conversation_id, 256),
        ] {
            if value.is_empty() || value.len() > limit || value.contains('\0') {
                return Err(GatewayError::Invalid);
            }
        }
        Ok(digest(&serde_json::to_vec(self)?))
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboundMessage {
    pub route: ChannelRoute,
    pub event_id: String,
    pub text: String,
}

impl InboundMessage {
    pub(crate) fn key(&self) -> GatewayResult<String> {
        self.route.key()?;
        if self.event_id.is_empty()
            || self.event_id.len() > 256
            || self.event_id.contains('\0')
            || self.text.len() > 32768
            || self.text.trim().is_empty()
        {
            return Err(GatewayError::Invalid);
        }
        // Adapter-global event deduplication survives changing selected devices.
        Ok(digest(&serde_json::to_vec(&(
            &self.route.identity.adapter_instance_id,
            &self.event_id,
        ))?))
    }

    pub(crate) fn fingerprint(&self) -> GatewayResult<String> {
        Ok(digest(&serde_json::to_vec(self)?))
    }
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptState {
    Processing,
    Submitted,
    Completed,
    Interrupted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GatewayReceipt {
    pub event_key: String,
    pub state: ReceiptState,
    pub session_id: Option<Uuid>,
    pub turn_id: Option<Uuid>,
}

/// Details are rendered as a separate dialog/card, never an ordinary AI reply.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InteractionCard {
    pub request_id: Uuid,
    pub turn_id: Uuid,
    pub session_id: Uuid,
    pub device_id: Uuid,
    pub device_name: String,
    pub workspace: String,
    pub expires_at: i64,
    pub context: InteractionContext,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeliveryBody {
    Reply { reply: ChatReply },
    Notice { text: String },
    Interaction { card: InteractionCard },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Delivery {
    pub delivery_id: Uuid,
    pub route: ChannelRoute,
    pub in_reply_to: String,
    /// Persisted before sending. QQ uses this with msg_id for deduplication.
    pub message_sequence: u32,
    pub body: DeliveryBody,
}

/// Deliberately carries no platform response body, credentials or request URL.
#[derive(Debug, thiserror::Error)]
pub enum DeliveryError {
    #[error("channel rejected delivery before accepting it")]
    Rejected,
    #[error("channel delivery result is unknown")]
    Unconfirmed,
}

#[async_trait]
pub trait MessageAdapter: Send + Sync {
    fn instance_id(&self) -> &str;
    async fn deliver(&self, delivery: &Delivery) -> Result<(), DeliveryError>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InteractionAction {
    AllowOnce {},
    Reject {},
    Answers { answers: Vec<QuestionAnswer> },
}
