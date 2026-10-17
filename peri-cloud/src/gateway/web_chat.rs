//! Persisted Web conversation projection. The gateway outbox remains the
//! delivery authority; `web_messages` is the browser-visible projection.

use std::sync::Arc;

use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use super::{
    ActivityPhase, ChannelRoute, DeviceReachability, Gateway, GatewayError, GatewayReceipt,
    GatewayResult, InboundMessage, InteractionCard, WEB_ADAPTER,
};
use crate::identity::{Authenticated, DeviceRecord};

pub(crate) const WEB_CONVERSATION: &str = "web:default";

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0)
}

#[derive(Clone, serde::Serialize)]
pub struct WebMessage {
    pub seq: i64,
    pub role: String,
    pub text: String,
    pub state: String,
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<InteractionCard>,
}

#[derive(serde::Serialize)]
pub struct WebDevice {
    pub id: Uuid,
    pub name: String,
    pub platform: String,
    pub default_workspace: String,
    pub connection: DeviceReachability,
}

#[derive(serde::Serialize)]
pub struct WebChat {
    pub conversation_id: String,
    pub selected: bool,
    pub session_id: Option<Uuid>,
    pub device_id: Option<Uuid>,
    pub device_name: Option<String>,
    pub workspace: Option<String>,
    pub phase: Option<ActivityPhase>,
    pub needs_device: bool,
    pub devices: Vec<WebDevice>,
    pub messages: Vec<WebMessage>,
    pub cursor: i64,
}

#[derive(serde::Serialize)]
pub struct WebSendResult {
    pub conversation_id: String,
    pub receipt: GatewayReceipt,
    pub cursor: i64,
}

#[derive(Clone)]
struct WebStore {
    pool: SqlitePool,
}

impl WebStore {
    fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    async fn append_user(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
        event_key: &str,
        text: &str,
        now: i64,
    ) -> GatewayResult<i64> {
        let route_key = route.key()?;
        sqlx::query("INSERT OR IGNORE INTO web_messages(route_key,principal,event_key,role,text,state,created_at) VALUES(?,?,?,'user',?,'pending',?)")
            .bind(&route_key)
            .bind(principal.to_string())
            .bind(event_key)
            .bind(text)
            .bind(now)
            .execute(&self.pool)
            .await?;
        let row = sqlx::query("SELECT seq FROM web_messages WHERE event_key=?")
            .bind(event_key)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get("seq"))
    }

    async fn settle_user(&self, event_key: &str, state: &str) -> GatewayResult<()> {
        sqlx::query("UPDATE web_messages SET state=? WHERE event_key=? AND state='pending'")
            .bind(state)
            .bind(event_key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// A crash between durable gateway admission and the pending->accepted
    /// update is reconciled from the gateway inbox. Old rows that never
    /// reached admission become visibly failed instead of pretending to send.
    async fn reconcile(&self, route: &ChannelRoute, now: i64) -> GatewayResult<()> {
        let route_key = route.key()?;
        sqlx::query("UPDATE web_messages SET state='accepted' WHERE route_key=? AND state='pending' AND event_key IN (SELECT event_key FROM gateway_inbox)")
            .bind(&route_key)
            .execute(&self.pool)
            .await?;
        sqlx::query("UPDATE web_messages SET state='failed' WHERE route_key=? AND state='pending' AND created_at<? AND event_key NOT IN (SELECT event_key FROM gateway_inbox)")
            .bind(&route_key)
            .bind(now.saturating_sub(300))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn history(
        &self,
        route: &ChannelRoute,
        after: Option<i64>,
        limit: usize,
    ) -> GatewayResult<(Vec<WebMessage>, i64)> {
        let route_key = route.key()?;
        let limit = limit.clamp(1, 200) as i64;
        let rows = if let Some(after) = after {
            sqlx::query("SELECT seq,role,text,state,created_at,card_json FROM web_messages WHERE route_key=? AND seq>? ORDER BY seq LIMIT ?")
                .bind(&route_key)
                .bind(after)
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
        } else {
            sqlx::query("SELECT seq,role,text,state,created_at,card_json FROM (SELECT seq,role,text,state,created_at,card_json FROM web_messages WHERE route_key=? ORDER BY seq DESC LIMIT ?) ORDER BY seq")
                .bind(&route_key)
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
        };
        let cursor = rows
            .last()
            .map(|row| row.get::<i64, _>("seq"))
            .or(after)
            .unwrap_or(0);
        let messages = rows
            .into_iter()
            .map(|row| {
                let card = row
                    .get::<Option<String>, _>("card_json")
                    .and_then(|value| serde_json::from_str::<InteractionCard>(&value).ok());
                WebMessage {
                    seq: row.get("seq"),
                    role: row.get("role"),
                    text: row.get("text"),
                    state: row.get("state"),
                    created_at: row.get("created_at"),
                    card,
                }
            })
            .collect();
        Ok((messages, cursor))
    }
}

impl Gateway {
    async fn prepare_web(
        &self,
        actor: &Authenticated,
    ) -> GatewayResult<(ChannelRoute, Uuid, Vec<DeviceRecord>)> {
        self.identity.browser_scope(actor).await?;
        if !self.adapters.contains_key(WEB_ADAPTER) {
            return Err(GatewayError::Invalid);
        }
        let principal = actor.principal().id;
        let identity = self.identity.web_identity(actor).await?;
        let route = ChannelRoute {
            identity,
            conversation_id: WEB_CONVERSATION.into(),
        };
        let devices = self.identity.devices(actor).await?;
        Ok((route, principal, devices))
    }

    pub async fn web_chat(
        &self,
        actor: &Authenticated,
        after: Option<i64>,
    ) -> GatewayResult<WebChat> {
        let (route, principal, devices) = self.prepare_web(actor).await?;
        let web = WebStore::new(self.runtime.journal().identity_pool());
        let now = unix_now();
        if after.is_none() {
            web.reconcile(&route, now).await?;
        }
        let (messages, cursor) = web.history(&route, after, 100).await?;
        let mut states = std::collections::HashMap::new();
        for device in &devices {
            states.insert(device.id, self.connector.reachability(device).await);
        }
        let available: Vec<&DeviceRecord> = devices.iter().filter(|d| !d.revoked).collect();
        let web_devices: Vec<WebDevice> = available
            .iter()
            .map(|device| WebDevice {
                id: device.id,
                name: device.name.clone(),
                platform: device.platform.clone(),
                default_workspace: device.default_workspace.clone(),
                connection: states.get(&device.id).copied().unwrap_or_default(),
            })
            .collect();
        let selected = self.store.selection(&route, principal).await?;
        let (mut session_id, mut device_id, mut device_name, mut workspace, mut phase) =
            (None, None, None, None, None);
        let mut needs_device = web_devices.is_empty() || web_devices.len() > 1;
        if let Some(session) = selected {
            let state = self.runtime.journal().session(principal, session).await?;
            let current = state.frozen.device_id;
            let available = available.iter().find(|device| device.id == current);
            session_id = Some(session);
            device_id = Some(current);
            device_name = Some(
                available
                    .map(|device| device.name.clone())
                    .unwrap_or_else(|| "已撤销设备".into()),
            );
            workspace = Some(state.frozen.binding.workspace.clone());
            let activity = self.browser_activity(actor).await?;
            phase = activity
                .iter()
                .find(|entry| entry.session_id == session)
                .map(|entry| entry.phase);
            needs_device = available.is_none();
        }
        Ok(WebChat {
            conversation_id: route.conversation_id,
            selected: session_id.is_some(),
            session_id,
            device_id,
            device_name,
            workspace,
            phase,
            needs_device,
            devices: web_devices,
            messages,
            cursor,
        })
    }

    /// Select the Web conversation's device without routing a fake command
    /// through QQ. The owned browser workspace path performs validation,
    /// settlement checks and route selection.
    pub async fn web_select(
        self: &Arc<Self>,
        actor: &Authenticated,
        device: Option<Uuid>,
        workspace: Option<String>,
    ) -> GatewayResult<Uuid> {
        let (route, principal, devices) = self.prepare_web(actor).await?;
        let available: Vec<&DeviceRecord> = devices.iter().filter(|d| !d.revoked).collect();
        let device = match device {
            Some(id) => available
                .iter()
                .find(|device| device.id == id)
                .copied()
                .ok_or(GatewayError::Invalid)?,
            None if available.len() == 1 => available[0],
            None => return Err(GatewayError::Invalid),
        };
        let workspace = workspace
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| device.default_workspace.clone());
        let selected = self.store.selection(&route, principal).await?;
        let frozen = self
            .select_instance(&route, principal, device, selected, Some(&workspace))
            .await?;
        Ok(frozen.session_id)
    }

    pub async fn web_send(
        self: &Arc<Self>,
        actor: &Authenticated,
        text: impl Into<String>,
    ) -> GatewayResult<WebSendResult> {
        let (route, principal, devices) = self.prepare_web(actor).await?;
        let text = text.into().trim().to_owned();
        if text.is_empty() || text.len() > 32768 {
            return Err(GatewayError::Invalid);
        }
        let selected = self.store.selection(&route, principal).await?;
        if let Some(session) = selected {
            let state = self.runtime.journal().session(principal, session).await?;
            if !devices
                .iter()
                .any(|device| device.id == state.frozen.device_id && !device.revoked)
            {
                return Err(GatewayError::Invalid);
            }
        } else if devices.iter().filter(|device| !device.revoked).count() != 1 {
            return Err(GatewayError::Invalid);
        }
        let message = InboundMessage {
            route: route.clone(),
            event_id: format!("web:{}", Uuid::new_v4()),
            text,
            reply_kind: Default::default(),
        };
        let key = message.key()?;
        let web = WebStore::new(self.runtime.journal().identity_pool());
        let now = unix_now();
        let cursor = web
            .append_user(&route, principal, &key, &message.text, now)
            .await?;
        let receipt = match self.receive(message).await {
            Ok(receipt) => {
                web.settle_user(&key, "accepted").await?;
                receipt
            }
            Err(GatewayError::RecoveryRequired) => return Err(GatewayError::RecoveryRequired),
            Err(error) => {
                web.settle_user(&key, "failed").await?;
                return Err(error);
            }
        };
        Ok(WebSendResult {
            conversation_id: route.conversation_id,
            receipt,
            cursor,
        })
    }
}
