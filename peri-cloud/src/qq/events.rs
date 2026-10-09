use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use super::{QqAdapter, QqError, QqResult};
use crate::gateway::{ChannelRoute, Gateway, InboundMessage, InteractionAction};
use crate::identity::ChannelIdentity;

pub enum QqInput {
    Message(InboundMessage),
    Interaction {
        route: ChannelRoute,
        event_id: String,
        request: Uuid,
        action: InteractionAction,
    },
}

#[derive(Deserialize)]
struct Author {
    user_openid: Option<String>,
    member_openid: Option<String>,
}
#[derive(Deserialize)]
struct Message {
    id: String,
    content: String,
    author: Author,
    group_openid: Option<String>,
}

pub fn normalize(instance: &str, event_type: &str, data: Value) -> QqResult<Option<QqInput>> {
    match event_type {
        "C2C_MESSAGE_CREATE" | "GROUP_AT_MESSAGE_CREATE" => {
            let message: Message = serde_json::from_value(data).map_err(|_| QqError::Event)?;
            let (user, conversation) = if event_type == "C2C_MESSAGE_CREATE" {
                let user = message.author.user_openid.ok_or(QqError::Event)?;
                let conversation = format!("c2c:{user}");
                (user, conversation)
            } else {
                let user = message.author.member_openid.ok_or(QqError::Event)?;
                let group = message
                    .group_openid
                    .filter(|value| valid_id(value))
                    .ok_or(QqError::Event)?;
                (user, format!("group:{group}"))
            };
            if !valid_id(&user) || !valid_event(&message.id) || message.content.len() > 32768 {
                return Err(QqError::Event);
            }
            if message.content.trim().is_empty() {
                return Ok(None);
            }
            Ok(Some(QqInput::Message(InboundMessage {
                route: route(instance, user, conversation),
                event_id: message.id,
                text: message.content,
            })))
        }
        "INTERACTION_CREATE" => {
            // Hermes reads data.type; QQ also duplicates it at the top level
            // in some payloads. Accept either documented shape, reject conflict.
            let nested = data["data"]["type"].as_u64();
            let top = data["type"].as_u64();
            if nested.is_some() && top.is_some() && nested != top {
                return Err(QqError::Event);
            }
            if nested.or(top) != Some(11) {
                return Ok(None);
            }
            let id = data["id"]
                .as_str()
                .filter(|value| valid_event(value))
                .ok_or(QqError::Event)?
                .to_owned();
            let (user, conversation) = match data["chat_type"].as_u64() {
                Some(2) => {
                    let user = data["user_openid"]
                        .as_str()
                        .ok_or(QqError::Event)?
                        .to_owned();
                    (user.clone(), format!("c2c:{user}"))
                }
                Some(1) => {
                    let user = data["group_member_openid"]
                        .as_str()
                        .ok_or(QqError::Event)?
                        .to_owned();
                    let group = data["group_openid"]
                        .as_str()
                        .filter(|value| valid_id(value))
                        .ok_or(QqError::Event)?;
                    (user, format!("group:{group}"))
                }
                _ => return Ok(None),
            };
            if !valid_id(&user) {
                return Err(QqError::Event);
            }
            // The resolved.user_id field is for guilds. Never use it as the
            // authenticated sender for a C2C/group interaction.
            let payload = data["data"]["resolved"]["button_data"]
                .as_str()
                .ok_or(QqError::Event)?;
            let parts: Vec<_> = payload.split(':').collect();
            if parts.len() != 3 || parts[0] != "peri" {
                return Err(QqError::Event);
            }
            let request = Uuid::parse_str(parts[2]).map_err(|_| QqError::Event)?;
            let action = match parts[1] {
                "allow" => InteractionAction::AllowOnce {},
                "reject" => InteractionAction::Reject {},
                _ => return Err(QqError::Event),
            };
            Ok(Some(QqInput::Interaction {
                route: route(instance, user, conversation),
                event_id: id,
                request,
                action,
            }))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
#[path = "events_test.rs"]
mod tests;

fn route(instance: &str, user: String, conversation_id: String) -> ChannelRoute {
    ChannelRoute {
        identity: ChannelIdentity {
            adapter_instance_id: instance.into(),
            external_user_id: user,
        },
        conversation_id,
    }
}
fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
}
fn valid_event(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

impl QqAdapter {
    pub(crate) async fn accept(
        &self,
        gateway: &std::sync::Arc<Gateway>,
        kind: &str,
        data: Value,
    ) -> QqResult<()> {
        if kind == "INTERACTION_CREATE" {
            tracing::info!(
                component = "qq_interaction",
                chat_type = data["chat_type"].as_u64(),
                has_user_openid = data["user_openid"].as_str().is_some(),
                has_group_member_openid = data["group_member_openid"].as_str().is_some(),
                "QQ interaction received"
            );
        }
        match normalize(&self.instance, kind, data)? {
            Some(QqInput::Message(message)) => {
                gateway
                    .receive(message)
                    .await
                    .map_err(|_| QqError::Admission)?;
            }
            Some(QqInput::Interaction {
                route,
                event_id,
                request,
                action,
            }) => {
                let accepted = gateway.respond(&route, request, action).await.is_ok();
                tracing::info!(
                    component = "qq_interaction",
                    accepted,
                    "QQ approval interaction processed"
                );
                let code = if accepted { 0 } else { 4 };
                // Approval is already single-consumed. ACK retry cannot replay it.
                self.acknowledge_interaction(&event_id, code).await?;
            }
            None => (),
        }
        Ok(())
    }
}
