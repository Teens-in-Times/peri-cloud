use peri_acp_types::interaction::InteractionContext;
use serde_json::{json, Value};

use crate::gateway::{Delivery, DeliveryBody, DeliveryError};

pub(super) fn payload(delivery: &Delivery) -> Result<(&'static str, Value), DeliveryError> {
    let text = match &delivery.body {
        DeliveryBody::Reply { reply } => crate::gateway::plain_chat(&reply.text),
        DeliveryBody::Notice { text } => text.clone(),
        DeliveryBody::Menu { card } => {
            let page = crate::gateway::menu_view::page(card, delivery.delivery_id, 1)
                .map_err(|_| DeliveryError::Rejected)?;
            let text = page.render().replace(
                "点击按钮，或发送 /选 编号；编号操作当前最新菜单。",
                "发送 /选 编号；编号操作当前最新菜单。",
            );
            format!(
                "{text}\n\n回复本菜单并发送编号，或发送 /选 编号。\n菜单引用：{}",
                delivery.delivery_id
            )
        }
        DeliveryBody::Interaction { card } => {
            let code = &card.request_id.simple().to_string()[..8];
            match &card.context {
                InteractionContext::Approval { items } => format!(
                    "确认 {} 项操作\n设备：{}\n工作区：{}\n工具：{}\n\n完整参数：/详情 {code}\n允许本次：/允许 {code}\n拒绝本次：/拒绝 {code}",
                    items.len(), line(&card.device_name), line(&card.workspace),
                    items.iter().map(|item|line(&item.tool_name)).collect::<Vec<_>>().join("、")
                ),
                InteractionContext::Questions { .. } => "Agent 需要你的回答。发送 /问题 打开完整题目与编号选项，或 /回答 内容。".into(),
            }
        }
    };
    let user = delivery
        .route
        .identity
        .external_user_id
        .parse::<i64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or(DeliveryError::Rejected)?;
    let mut segments = Vec::new();
    // Group replies quote the triggering message so the answer stays anchored in
    // a busy thread. Private chats send a standalone message: a reply segment
    // there turns every answer into a quote bubble under the user's message.
    let group = delivery.route.conversation_id.strip_prefix("group:");
    if group.is_some() {
        // Reply IDs originate in our own normalized event identity.
        if let Some(id) = (delivery.reply_kind != crate::gateway::ReplyKind::Proactive)
            .then(|| {
                delivery
                    .in_reply_to
                    .rsplit(':')
                    .next()
                    .and_then(|s| s.parse::<i64>().ok())
            })
            .flatten()
        {
            segments.push(json!({"type":"reply","data":{"id":id.to_string()}}));
        }
    }
    let (action, mut params) = if let Some(group) = group {
        let group: i64 = group
            .parse()
            .ok()
            .filter(|n| *n > 0)
            .ok_or(DeliveryError::Rejected)?;
        segments.push(json!({"type":"at","data":{"qq":user.to_string()}}));
        ("send_group_msg", json!({"group_id":group}))
    } else if delivery.route.conversation_id == format!("private:{user}") {
        ("send_private_msg", json!({"user_id":user}))
    } else {
        return Err(DeliveryError::Rejected);
    };
    // Text segments prevent model or tool text from becoming executable CQ codes.
    segments.push(json!({"type":"text","data":{"text":text}}));
    params["message"] = Value::Array(segments);
    Ok((action, params))
}

fn line(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[cfg(test)]
#[path = "render_test.rs"]
mod tests;
