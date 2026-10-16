use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use url::Url;

use super::{QqError, QqResult};
use crate::gateway::{Delivery, DeliveryBody, DeliveryError, MessageAdapter};

#[derive(Clone)]
pub struct QqEndpoints {
    pub(crate) api: Url,
    pub(crate) token: Url,
}

impl Default for QqEndpoints {
    fn default() -> Self {
        Self {
            api: "https://api.sgroup.qq.com/".parse().unwrap(),
            token: "https://bots.qq.com/app/getAppAccessToken".parse().unwrap(),
        }
    }
}

impl QqEndpoints {
    /// Deployment-local relay endpoints, useful for protocol fixtures. Production
    /// credentials otherwise go only to the fixed official QQ endpoints.
    pub fn loopback(api: Url, token: Url) -> QqResult<Self> {
        for url in [&api, &token] {
            if url.scheme() != "http"
                || !matches!(url.host_str(), Some("127.0.0.1" | "[::1]"))
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(QqError::Configuration);
            }
        }
        if api.path() != "/" {
            return Err(QqError::Configuration);
        }
        Ok(Self { api, token })
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum QqInteractionMode {
    /// Raw Markdown and custom keyboard, as used by Hermes's QQ adapter.
    Native {},
    Web {},
}

impl Default for QqInteractionMode {
    fn default() -> Self {
        Self::Native {}
    }
}

struct CachedToken {
    value: String,
    expires: Instant,
}

pub struct QqAdapter {
    pub(crate) instance: String,
    pub(crate) app_id: String,
    pub(crate) secret: String,
    pub(crate) endpoints: QqEndpoints,
    pub(crate) client: Client,
    pub(crate) web_origin: String,
    interaction: QqInteractionMode,
    cached: Mutex<Option<CachedToken>>,
    refresh_gate: tokio::sync::Mutex<()>,
    native_rejected: AtomicBool,
}

impl QqAdapter {
    pub fn new(
        instance: String,
        app_id: String,
        secret: String,
        web_origin: &str,
        interaction: QqInteractionMode,
        endpoints: QqEndpoints,
    ) -> QqResult<Arc<Self>> {
        if instance.is_empty()
            || instance.len() > 128
            || secret.is_empty()
            || secret.len() > 1024
            || app_id.is_empty()
            || !app_id.bytes().all(|c| c.is_ascii_digit())
        {
            return Err(QqError::Configuration);
        }
        let portal =
            crate::portal::PortalConfig::new(web_origin).map_err(|_| QqError::Configuration)?;
        let mut builder = Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none());
        if endpoints.api.scheme() == "http" {
            builder = builder.no_proxy();
        }
        let client = builder.build().map_err(|_| QqError::Configuration)?;
        Ok(Arc::new(Self {
            instance,
            app_id,
            secret,
            endpoints,
            client,
            web_origin: portal.origin,
            interaction,
            cached: Mutex::new(None),
            refresh_gate: tokio::sync::Mutex::new(()),
            native_rejected: AtomicBool::new(false),
        }))
    }

    pub(crate) async fn access_token(&self) -> QqResult<String> {
        if let Some(value) = self.fresh_token() {
            return Ok(value);
        }
        let _gate = self.refresh_gate.lock().await;
        if let Some(value) = self.fresh_token() {
            return Ok(value);
        }
        let response = self
            .client
            .post(self.endpoints.token.clone())
            .json(&json!({"appId":self.app_id,"clientSecret":self.secret}))
            .send()
            .await
            .map_err(|_| QqError::Connection)?;
        if !response.status().is_success() {
            return Err(QqError::Authentication);
        }
        let body: Value = response.json().await.map_err(|_| QqError::Connection)?;
        let value = body["access_token"]
            .as_str()
            .filter(|value| !value.is_empty() && value.len() <= 4096)
            .ok_or(QqError::Authentication)?
            .to_owned();
        let lifetime = body["expires_in"]
            .as_u64()
            .or_else(|| {
                body["expires_in"]
                    .as_str()
                    .and_then(|value| value.parse().ok())
            })
            .filter(|seconds| (1..=30 * 86400).contains(seconds))
            .ok_or(QqError::Authentication)?;
        // Refresh ahead of expiry without treating very short tokens as stale.
        let usable = lifetime.saturating_sub(60).max(1).min(lifetime);
        *self.cached.lock() = Some(CachedToken {
            value: value.clone(),
            expires: Instant::now() + Duration::from_secs(usable),
        });
        Ok(value)
    }

    fn fresh_token(&self) -> Option<String> {
        self.cached
            .lock()
            .as_ref()
            .filter(|token| token.expires > Instant::now())
            .map(|token| token.value.clone())
    }

    pub(crate) async fn gateway_url(&self) -> QqResult<Url> {
        let token = self.access_token().await?;
        let response = self
            .client
            .get(self.endpoints.api.join("gateway").unwrap())
            .header("Authorization", format!("QQBot {token}"))
            .send()
            .await
            .map_err(|_| QqError::Connection)?;
        if !response.status().is_success() {
            return Err(QqError::Authentication);
        }
        let value: Value = response.json().await.map_err(|_| QqError::Connection)?;
        let url: Url = value["url"]
            .as_str()
            .ok_or(QqError::Connection)?
            .parse()
            .map_err(|_| QqError::Connection)?;
        let accepted = if self.endpoints.api.scheme() == "http" {
            url.scheme() == "ws" && matches!(url.host_str(), Some("127.0.0.1" | "[::1]"))
        } else {
            url.scheme() == "wss" && url.host_str() == Some("api.sgroup.qq.com")
        };
        if !accepted
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(QqError::Connection);
        }
        Ok(url)
    }

    pub(crate) async fn acknowledge_interaction(&self, id: &str, code: u8) -> QqResult<()> {
        let mut url = self.endpoints.api.clone();
        url.path_segments_mut()
            .map_err(|_| QqError::Configuration)?
            .extend(["interactions", id]);
        let token = self.access_token().await?;
        let response = self
            .client
            .put(url)
            .header("Authorization", format!("QQBot {token}"))
            .json(&json!({"code":code}))
            .send()
            .await
            .map_err(|_| QqError::Connection)?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(QqError::Connection)
        }
    }

    fn body(&self, delivery: &Delivery) -> QqResult<Value> {
        self.payload(delivery, !self.native_rejected.load(Ordering::Relaxed))
    }

    fn payload(&self, delivery: &Delivery, native: bool) -> QqResult<Value> {
        let text = match &delivery.body {
            DeliveryBody::Reply { reply } => reply.text.clone(),
            DeliveryBody::Notice { text } => text.clone(),
            DeliveryBody::Menu { card } => menu_text(
                card,
                delivery.delivery_id,
                native && matches!(self.interaction, QqInteractionMode::Native {}),
            )?,
            DeliveryBody::Interaction { card } => super::card::text(
                card,
                &self.web_origin,
                native && matches!(self.interaction, QqInteractionMode::Native {}),
            ),
        };
        let mut body = json!({"content":text,"msg_type":0,"msg_id":delivery.in_reply_to,"msg_seq":delivery.message_sequence});
        if let (true, DeliveryBody::Interaction { card }, QqInteractionMode::Native {}) =
            (native, &delivery.body, &self.interaction)
        {
            let buttons = match &card.context {
                peri_acp_types::interaction::InteractionContext::Approval { .. } => vec![
                    button(
                        "allow",
                        "允许一次",
                        &format!("peri:allow:{}", card.request_id),
                    ),
                    button(
                        "reject",
                        "拒绝",
                        &format!("peri:reject:{}", card.request_id),
                    ),
                ],
                peri_acp_types::interaction::InteractionContext::Questions { requests } => {
                    let mut buttons = Vec::new();
                    if requests.len() == 1 && !requests[0].multi_select {
                        for (index, option) in requests[0].options.iter().take(6).enumerate() {
                            buttons.push(button(
                                &format!("answer-{index}"),
                                &menu_label(index, &option.label, 3),
                                &format!("peri:answer:{}:{index}", card.request_id),
                            ));
                        }
                    }
                    buttons.push(button(
                        "reject",
                        "暂不回答",
                        &format!("peri:reject:{}", card.request_id),
                    ));
                    buttons
                }
            };
            let rows: Vec<_> = buttons
                .chunks(3)
                .map(|row| json!({"buttons":row}))
                .collect();
            body = json!({"msg_type":2,"msg_id":delivery.in_reply_to,"msg_seq":delivery.message_sequence,
            "markdown":{"content":text},
            "keyboard":{"content":{"rows":rows}}});
        }
        if let (true, DeliveryBody::Menu { card }, QqInteractionMode::Native {}) =
            (native, &delivery.body, &self.interaction)
        {
            let columns = menu_columns(card);
            let rows:Vec<_>=card.buttons.chunks(columns).enumerate().map(|(row_index,row)|json!({"buttons":row.iter().enumerate().map(|(index,item)| {
                let index = row_index * columns + index;
                button(&format!("menu-{index}"), &menu_label(index, &item.label, columns), &format!("peri:menu:{}",item.action_id))
            }).collect::<Vec<_>>() })).collect();
            body = json!({"msg_type":2,"msg_id":delivery.in_reply_to,"msg_seq":delivery.message_sequence,"markdown":{"content":text},"keyboard":{"content":{"rows":rows}}});
        }
        if delivery.reply_kind == crate::gateway::ReplyKind::Event {
            body.as_object_mut().unwrap().remove("msg_id");
            body["event_id"] = json!(delivery.in_reply_to);
        }
        Ok(body)
    }
}

fn menu_text(
    card: &crate::gateway::MenuCard,
    snapshot: uuid::Uuid,
    markdown: bool,
) -> QqResult<String> {
    let text = crate::gateway::menu_view::page(card, snapshot, 1)
        .map_err(|_| QqError::Configuration)?
        .render();
    if !markdown {
        return Ok(text);
    }
    Ok(text
        .lines()
        .map(|line| super::card::excerpt(line, usize::MAX, true))
        .collect::<Vec<_>>()
        .join("\n"))
}

fn label_width(label: &str) -> usize {
    label
        .chars()
        .map(|ch| if ch.is_ascii() { 1 } else { 2 })
        .sum()
}

fn menu_columns(card: &crate::gateway::MenuCard) -> usize {
    // Five rows is a platform limit. Fewer columns leave ordinary actions
    // readable; long names live in the complete numbered text, not an ellipsis.
    if card.buttons.len() <= 5
        && card
            .buttons
            .iter()
            .any(|item| label_width(&item.label) > 20)
    {
        1
    } else if card.buttons.len() <= 10 {
        2
    } else {
        3
    }
}

fn menu_label(index: usize, label: &str, columns: usize) -> String {
    let label: String = label
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let numbered = format!("{} · {label}", index + 1);
    let budget = match columns {
        1 => 40,
        2 => 24,
        _ => 16,
    };
    if label_width(&numbered) <= budget {
        numbered
    } else {
        format!("选 {}", index + 1)
    }
}

fn button(id: &str, label: &str, data: &str) -> Value {
    // Match Hermes: let clicks reach the gateway, which authenticates the
    // observed sender against the exact pending route before consuming approval.
    json!({"id":id,"group_id":if id.starts_with("menu-"){"menu"}else{"approval"},"render_data":{"label":label,"visited_label":label,"style":if id == "reject" { 0 } else { 1 }},"action":{"type":1,"permission":{"type":2},"data":data,"unsupport_tips":"请使用正文中的 /选、/回答、/允许 或 /拒绝 指令。"}})
}

#[async_trait]
impl MessageAdapter for QqAdapter {
    fn initial_access_policy(&self) -> crate::gateway::access::AccessPolicy {
        crate::gateway::access::AccessPolicy::open()
    }
    fn conversation_scope(
        &self,
        route: &crate::gateway::ChannelRoute,
    ) -> crate::gateway::access::ConversationScope {
        use crate::gateway::access::ConversationScope;
        if let Some(group) = route.conversation_id.strip_prefix("group:") {
            ConversationScope::Group(group.into())
        } else if route.conversation_id == format!("c2c:{}", route.identity.external_user_id) {
            ConversationScope::Private
        } else {
            ConversationScope::Unknown
        }
    }
    fn instance_id(&self) -> &str {
        &self.instance
    }
    async fn deliver(&self, delivery: &Delivery) -> Result<(), DeliveryError> {
        if delivery.route.identity.adapter_instance_id != self.instance
            || delivery.message_sequence == 0
        {
            return Err(DeliveryError::Rejected);
        }
        let (kind, target) = delivery
            .route
            .conversation_id
            .split_once(':')
            .filter(|(kind, target)| matches!(*kind, "c2c" | "group") && !target.is_empty())
            .ok_or(DeliveryError::Rejected)?;
        if kind == "c2c" && target != delivery.route.identity.external_user_id {
            return Err(DeliveryError::Rejected);
        }
        let mut url = self.endpoints.api.clone();
        url.path_segments_mut()
            .map_err(|_| DeliveryError::Rejected)?
            .extend([
                "v2",
                if kind == "c2c" { "users" } else { "groups" },
                target,
                "messages",
            ]);
        let token = self
            .access_token()
            .await
            .map_err(|_| DeliveryError::Rejected)?;
        let body = self.body(delivery).map_err(|_| DeliveryError::Rejected)?;
        let response = self
            .client
            .post(url.clone())
            .header("Authorization", format!("QQBot {token}"))
            .json(&body)
            .send()
            .await
            .map_err(|_| DeliveryError::Unconfirmed)?;
        // Only an explicit platform rejection permits a fallback submission.
        // A timeout, 5xx, or unreadable success may already have sent the card.
        let fallback = body.get("keyboard").is_some()
            && matches!(
                response.status(),
                StatusCode::BAD_REQUEST | StatusCode::FORBIDDEN
            );
        let response = if fallback {
            let plain = self
                .payload(delivery, false)
                .map_err(|_| DeliveryError::Rejected)?;
            self.client
                .post(url)
                .header("Authorization", format!("QQBot {token}"))
                .json(&plain)
                .send()
                .await
                .map_err(|_| DeliveryError::Unconfirmed)?
        } else {
            response
        };
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED {
            *self.cached.lock() = None;
        }
        if status.is_client_error() {
            return Err(DeliveryError::Rejected);
        }
        if !status.is_success() {
            return Err(DeliveryError::Unconfirmed);
        }
        let value: Value = response
            .json()
            .await
            .map_err(|_| DeliveryError::Unconfirmed)?;
        if value["id"].as_str().is_some_and(|id| !id.is_empty()) {
            if fallback {
                self.native_rejected.store(true, Ordering::Relaxed);
            }
            Ok(())
        } else {
            Err(DeliveryError::Unconfirmed)
        }
    }
}

#[cfg(test)]
#[path = "api_test.rs"]
mod tests;
