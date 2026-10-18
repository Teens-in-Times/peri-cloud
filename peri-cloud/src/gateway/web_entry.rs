//! Internal structured Web entry. Browser authentication creates its identity;
//! it has no public message-ingress endpoint and performs no external delivery.
//! Deliveries are projected into the durable Web chat table for polling.
use crate::gateway::{Delivery, DeliveryBody, DeliveryError, MessageAdapter};
use crate::state::CloudJournal;
use async_trait::async_trait;
use peri_acp_types::interaction::InteractionContext;
use sqlx::SqlitePool;
use uuid::Uuid;

pub(crate) const WEB_ADAPTER: &str = "__peri_web";

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0)
}

pub struct WebAdapter {
    pool: SqlitePool,
}

impl WebAdapter {
    pub fn from_journal(journal: &CloudJournal) -> Self {
        Self {
            pool: journal.identity_pool(),
        }
    }

    async fn project(&self, delivery: &Delivery) -> Result<(), DeliveryError> {
        if delivery.route.identity.adapter_instance_id != WEB_ADAPTER {
            return Err(DeliveryError::Rejected);
        }
        let route_key = delivery.route.key().map_err(|_| DeliveryError::Rejected)?;
        let principal = Uuid::parse_str(&delivery.route.identity.external_user_id)
            .map_err(|_| DeliveryError::Rejected)?;
        let (role, text, card_json) = match &delivery.body {
            DeliveryBody::Reply { reply } => ("assistant", reply.text.clone(), None),
            DeliveryBody::Notice { text } => ("notice", text.clone(), None),
            DeliveryBody::Menu { card } => {
                ("notice", format!("{}\n{}", card.title, card.text), None)
            }
            DeliveryBody::Interaction { card } => {
                let summary = match &card.context {
                    InteractionContext::Approval { items } => {
                        format!("需要你确认 {} 项操作", items.len())
                    }
                    InteractionContext::Questions { .. } => "Agent 需要你的回答".into(),
                };
                ("interaction", summary, serde_json::to_string(card).ok())
            }
        };
        sqlx::query("INSERT OR IGNORE INTO web_messages(route_key,principal,delivery_id,role,text,card_json,state,created_at) VALUES(?,?,?,?,?,?,'accepted',?)")
            .bind(route_key)
            .bind(principal.to_string())
            .bind(delivery.delivery_id.to_string())
            .bind(role)
            .bind(text)
            .bind(card_json)
            .bind(unix_now())
            .execute(&self.pool)
            .await
            .map_err(|error| {
                tracing::error!(%error, "could not persist Web channel delivery");
                DeliveryError::Unconfirmed
            })?;
        Ok(())
    }
}

#[async_trait]
impl MessageAdapter for WebAdapter {
    fn conversation_scope(
        &self,
        route: &crate::gateway::ChannelRoute,
    ) -> super::access::ConversationScope {
        if route.identity.adapter_instance_id == WEB_ADAPTER
            && Uuid::parse_str(&route.identity.external_user_id).is_ok()
            && (route.conversation_id == super::web_chat::WEB_CONVERSATION
                || route
                    .conversation_id
                    .strip_prefix("workspace:")
                    .is_some_and(|id| Uuid::parse_str(id).is_ok()))
        {
            super::access::ConversationScope::Private
        } else {
            super::access::ConversationScope::Unknown
        }
    }
    fn supports_proactive(&self) -> bool {
        true
    }
    fn instance_id(&self) -> &str {
        WEB_ADAPTER
    }
    fn display_name(&self) -> &'static str {
        "网页任务入口"
    }
    fn connection_status(&self) -> (&'static str, Option<i64>) {
        ("browser_authenticated", None)
    }
    fn initial_access_policy(&self) -> crate::gateway::access::AccessPolicy {
        crate::gateway::access::AccessPolicy::open()
    }
    async fn deliver(&self, delivery: &Delivery) -> Result<(), DeliveryError> {
        self.project(delivery).await
    }
}
