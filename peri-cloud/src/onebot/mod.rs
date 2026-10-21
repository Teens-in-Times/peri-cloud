//! Optional personal QQ entry. OneBot owns transport only; Gateway remains the
//! single authority for accounts, conversations, menus and tool approvals.
mod events;
mod render;
mod wire;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::gateway::{Delivery, DeliveryError, MessageAdapter};

pub use events::normalize;

pub struct OneBotAdapter {
    instance: String,
    self_id: String,
    token: String,
    initial_access: crate::gateway::access::AccessPolicy,
    connection: Mutex<Option<Connection>>,
}

struct Connection {
    id: Uuid,
    requests: mpsc::Sender<ApiRequest>,
}

struct ApiRequest {
    action: &'static str,
    params: Value,
    response: oneshot::Sender<Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum OneBotError {
    #[error("OneBot configuration is invalid")]
    Configuration,
    #[error("OneBot listener or owned worker failed")]
    Worker,
}

impl OneBotAdapter {
    pub fn new(
        instance: String,
        self_id: String,
        token: String,
        initial_access: crate::gateway::access::AccessPolicy,
    ) -> Result<Arc<Self>, OneBotError> {
        initial_access
            .validate()
            .map_err(|_| OneBotError::Configuration)?;
        if instance.is_empty()
            || instance.len() > 128
            || instance.chars().any(char::is_control)
            || !events::valid_id(&self_id)
            || !(32..=8192).contains(&token.len())
            || !token.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(OneBotError::Configuration);
        }
        Ok(Arc::new(Self {
            instance,
            self_id,
            token,
            initial_access,
            connection: Mutex::new(None),
        }))
    }

    /// echo correlates a response; it is NOT platform idempotency. A sent call
    /// is never retried after timeout, disconnect or malformed acknowledgement.
    async fn request(&self, action: &'static str, params: Value) -> Result<Value, DeliveryError> {
        let (response, result) = oneshot::channel();
        {
            let connection = self.connection.lock();
            let connection = connection.as_ref().ok_or(DeliveryError::Rejected)?;
            connection
                .requests
                .try_send(ApiRequest {
                    action,
                    params,
                    response,
                })
                .map_err(|_| DeliveryError::Rejected)?;
        }
        tokio::time::timeout(Duration::from_secs(15), result)
            .await
            .map_err(|_| DeliveryError::Unconfirmed)?
            .map_err(|_| DeliveryError::Unconfirmed)
    }

    async fn inbound(self: Arc<Self>, gateway: Arc<crate::gateway::Gateway>, value: Value) {
        let Some(mut input) = events::parse(&self.instance, &self.self_id, &value) else {
            return;
        };
        // Deny before API lookups, Gateway admission, pairing or any model call.
        if !matches!(gateway.route_allowed(&input.message.route).await, Ok(true)) {
            return;
        }
        if let Some(reply) = input.quoted.as_ref() {
            // Read the original from the authenticated protocol endpoint rather
            // than trusting reply text embedded in a user-controlled event.
            let Ok(original) = self.request("get_msg", json!({"message_id": reply})).await else {
                return;
            };
            if events::message_id(&original["data"]["message_id"]) != Some(*reply) {
                return;
            }
            let Some(snapshot) = events::quoted_menu(&original, &self.self_id) else {
                return;
            };
            let Ok(index) = input.message.text.trim().parse::<usize>() else {
                return;
            };
            input.message.text = format!("/选 {snapshot} {index}");
            input.addressed = true;
        }
        if !input.addressed {
            return;
        }
        if gateway.receive(input.message).await.is_err() {
            // Do not log message text, platform payloads or bearer credentials.
            tracing::warn!(component = "onebot", "OneBot event admission failed");
        }
    }
}

#[async_trait]
impl MessageAdapter for OneBotAdapter {
    fn context_constraints(&self, _route: &crate::gateway::ChannelRoute) -> &'static str {
        "Channel: personal QQ account. Reply naturally and warmly, matching the conversation and the assistant's personality. Focus on meaningful progress, actual results and decisions needing the user's attention."
    }
    fn supports_proactive(&self) -> bool {
        true
    }
    fn display_name(&self) -> &'static str {
        "个人 QQ"
    }
    fn connection_status(&self) -> (&'static str, Option<i64>) {
        (
            if self
                .connection
                .lock()
                .as_ref()
                .is_some_and(|c| !c.requests.is_closed())
            {
                "bridge_authenticated"
            } else {
                "disconnected"
            },
            None,
        )
    }

    fn instance_id(&self) -> &str {
        &self.instance
    }
    fn initial_access_policy(&self) -> crate::gateway::access::AccessPolicy {
        self.initial_access.clone()
    }
    fn conversation_scope(
        &self,
        route: &crate::gateway::ChannelRoute,
    ) -> crate::gateway::access::ConversationScope {
        use crate::gateway::access::ConversationScope;
        if let Some(group) = route.conversation_id.strip_prefix("group:") {
            ConversationScope::Group(group.into())
        } else if route.conversation_id == format!("private:{}", route.identity.external_user_id) {
            ConversationScope::Private
        } else {
            ConversationScope::Unknown
        }
    }
    fn valid_access_id(&self, _list: crate::gateway::access::AccessList, id: &str) -> bool {
        events::valid_id(id)
    }

    async fn deliver(&self, delivery: &Delivery) -> Result<(), DeliveryError> {
        if delivery.route.identity.adapter_instance_id != self.instance {
            return Err(DeliveryError::Rejected);
        }
        let (action, params) = render::payload(delivery)?;
        let response = self.request(action, params).await?;
        match response["status"].as_str() {
            Some("ok")
                if response["retcode"].as_i64() == Some(0)
                    && events::message_id(&response["data"]["message_id"]).is_some() =>
            {
                Ok(())
            }
            // Only documented pre-execution validation/auth/action rejection
            // proves failure. Generic execution errors can follow a send.
            Some("failed")
                if matches!(
                    response["retcode"].as_i64(),
                    Some(100 | 1400 | 1401 | 1403 | 1404)
                ) =>
            {
                Err(DeliveryError::Rejected)
            }
            _ => Err(DeliveryError::Unconfirmed),
        }
    }
}

#[cfg(test)]
#[path = "mod_test.rs"]
mod tests;
