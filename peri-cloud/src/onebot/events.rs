use serde_json::Value;
use uuid::Uuid;

use crate::gateway::{ChannelRoute, InboundMessage, ReplyKind};
use crate::identity::ChannelIdentity;

pub(super) struct Input {
    pub message: InboundMessage,
    pub quoted: Option<i64>,
    pub addressed: bool,
}

pub(super) fn valid_id(id: &str) -> bool {
    id.parse::<i64>()
        .is_ok_and(|n| n > 0 && n.to_string() == id)
}

fn id(value: &Value) -> Option<String> {
    let id = value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_i64().map(|n| n.to_string()))?;
    valid_id(&id).then_some(id)
}

pub(super) fn message_id(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_str()?.parse().ok())
}

/// Normalize an explicitly addressed message. Transport uses parse() as well
/// to resolve a numeric reply to a bot menu before deciding group admission.
pub fn normalize(instance: &str, self_id: &str, value: &Value) -> Option<InboundMessage> {
    let input = parse(instance, self_id, value)?;
    input.addressed.then_some(input.message)
}

pub(super) fn parse(instance: &str, self_id: &str, value: &Value) -> Option<Input> {
    if value["post_type"] != "message" || id(&value["self_id"])? != self_id {
        return None;
    }
    let user = id(&value["user_id"])?;
    if user == self_id || !value["anonymous"].is_null() {
        return None;
    }
    let group = match value["message_type"].as_str()? {
        "private" if value["sub_type"] == "friend" => None,
        "group" if value["sub_type"] == "normal" => Some(id(&value["group_id"])?),
        _ => return None,
    };
    let msg_id = message_id(&value["message_id"])?;
    let (text, mentioned, quoted) = content(&value["message"], self_id)?;
    let text = text.trim().to_owned();
    if text.is_empty() || text.len() > 32768 {
        return None;
    }
    let addressed = group.is_none() || mentioned || command(&text);
    // Only a number can select a quoted menu. Other quoted group chatter is
    // ignored unless it also explicitly addresses this bot.
    let quoted = quoted.filter(|_| text.parse::<usize>().is_ok());
    if !addressed && quoted.is_none() {
        return None;
    }
    let conversation_id = group
        .map(|g| format!("group:{g}"))
        .unwrap_or_else(|| format!("private:{user}"));
    Some(Input {
        message: InboundMessage {
            event_id: format!("{conversation_id}:{msg_id}"),
            route: ChannelRoute {
                identity: ChannelIdentity {
                    adapter_instance_id: instance.into(),
                    external_user_id: user,
                },
                conversation_id,
            },
            text,
            reply_kind: ReplyKind::Message,
        },
        quoted,
        addressed,
    })
}

fn command(text: &str) -> bool {
    let verb = text.split_whitespace().next().unwrap_or_default();
    matches!(
        verb,
        "菜单"
            | "实例"
            | "工作区"
            | "/网关"
            | "/访问控制"
            | "/取消设置"
            | "/设备"
            | "/停止"
            | "/status"
            | "/stop"
            | "/result"
            | "/help"
            | "/菜单"
            | "/帮助"
            | "/实例"
            | "/电脑"
            | "/工作区"
            | "/连接"
            | "/绑定"
            | "/选"
            | "/菜单详情"
            | "/卡片"
            | "/权限"
            | "/状态"
            | "/取消"
            | "/允许"
            | "/允许一次"
            | "/拒绝"
            | "/详情"
            | "/回答"
            | "/问题"
            | "/结果"
            | "/设置"
            | "/记忆"
            | "/记住"
            | "/偏好"
            | "/用量"
            | "/模型"
    )
}

fn content(message: &Value, self_id: &str) -> Option<(String, bool, Option<i64>)> {
    if let Some(text) = message.as_str() {
        // Never interpret unparsed CQ markup as text or as an authenticated @.
        return (!text.contains("[CQ:")).then(|| (text.into(), false, None));
    }
    let segments = message.as_array()?;
    if segments.len() > 128 {
        return None;
    }
    let mut text = String::new();
    let mut mentioned = false;
    let mut quoted = None;
    for segment in segments {
        match segment["type"].as_str()? {
            "text" => text.push_str(segment["data"]["text"].as_str()?),
            "at" if id(&segment["data"]["qq"]).as_deref() == Some(self_id) => mentioned = true,
            "reply" if quoted.is_none() => quoted = Some(message_id(&segment["data"]["id"])?),
            // Do not silently feed an incomplete task after dropping an image,
            // file, another user's @ or other unsupported message components.
            _ => return None,
        }
    }
    Some((text, mentioned, quoted))
}

pub(super) fn quoted_menu(response: &Value, self_id: &str) -> Option<Uuid> {
    if response["status"] != "ok"
        || response["retcode"].as_i64() != Some(0)
        || id(&response["data"]["sender"]["user_id"])? != self_id
    {
        return None;
    }
    let message = &response["data"]["message"];
    let text = if let Some(text) = message.as_str() {
        text.to_owned()
    } else {
        let mut text = String::new();
        for segment in message.as_array()? {
            match segment["type"].as_str()? {
                "text" => text.push_str(segment["data"]["text"].as_str()?),
                "reply" | "at" => (),
                _ => return None,
            }
        }
        text
    };
    text.lines()
        .last()?
        .strip_prefix("菜单引用：")
        .and_then(|id| Uuid::parse_str(id).ok())
}

#[cfg(test)]
#[path = "events_test.rs"]
mod tests;
