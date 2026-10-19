//! Adapters authenticate platform facts; this boundary supplies uniform ingress semantics.
use super::{GatewayResult, InboundMessage};
use uuid::Uuid;

pub struct NormalizedMessage {
    pub event_key: String,
    pub principal: Option<Uuid>,
    pub message: InboundMessage,
}
pub struct MessageNormalizer;
impl MessageNormalizer {
    pub fn normalize(
        message: InboundMessage,
        principal: Option<Uuid>,
    ) -> GatewayResult<NormalizedMessage> {
        // Preserve exact user text and original fingerprint. Command aliases are
        // interpreted by the router after durable admission, not by adapters.
        let event_key = message.key()?;
        Ok(NormalizedMessage {
            event_key,
            principal,
            message,
        })
    }
}
