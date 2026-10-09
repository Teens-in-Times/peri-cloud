//! Bounded approval summaries. Exact parameters stay in the authenticated portal.

use peri_acp_types::interaction::{ApprovalItem, InteractionContext};

use crate::gateway::InteractionCard;

pub(super) fn text(card: &InteractionCard, origin: &str, markdown: bool) -> String {
    let link = format!("{origin}/?approval={}", card.request_id);
    let title = match &card.context {
        InteractionContext::Approval { items } => format!("确认 {} 项操作", items.len()),
        InteractionContext::Questions { .. } => "Agent 需要你的回答".into(),
    };
    let mut lines = vec![
        if markdown {
            format!("**{title}**")
        } else {
            title
        },
        format!(
            "{} · {}",
            excerpt(&card.device_name, 40, markdown),
            excerpt(&card.workspace, 80, markdown)
        ),
    ];
    if let InteractionContext::Approval { items } = &card.context {
        for item in items.iter().take(3) {
            lines.push(summary(item, markdown));
        }
        if items.len() > 3 {
            lines.push(format!("另有 {} 项操作，查看详情", items.len() - 3));
        }
    }
    lines.push(if markdown {
        format!("[查看完整参数]({link})")
    } else {
        format!("查看完整参数：{link}")
    });
    lines.join("\n")
}

fn summary(item: &ApprovalItem, markdown: bool) -> String {
    let label = match item.tool_name.as_str() {
        "Read" => "读取",
        "Write" => "写入",
        "Edit" => "编辑",
        "Glob" => "查找",
        "Grep" => "搜索",
        "Bash" => "命令",
        other => other,
    };
    let input = &item.tool_input;
    let target = if item.tool_name == "Bash" {
        input.get("command")
    } else {
        input
            .get("file_path")
            .or_else(|| input.get("pattern"))
            .or_else(|| input.get("path"))
    }
    .and_then(|value| value.as_str())
    .unwrap_or("查看详情");
    let size = if item.tool_name == "Write" {
        input
            .get("content")
            .and_then(|v| v.as_str())
            .map(|s| format!(" · {} 字符", s.chars().count()))
            .unwrap_or_default()
    } else {
        String::new()
    };
    format!(
        "{}：{}{size}",
        excerpt(label, 32, markdown),
        excerpt(target, 120, markdown)
    )
}

fn excerpt(value: &str, limit: usize, markdown: bool) -> String {
    let mut result = String::new();
    for (index, ch) in value.chars().enumerate() {
        if index == limit {
            result.push('…');
            break;
        }
        let ch = if ch.is_control() || ch.is_whitespace() {
            ' '
        } else {
            ch
        };
        if markdown && "\\`*_{}[]()<>#!|~".contains(ch) {
            result.push('\\');
        }
        result.push(ch);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;

    fn card(items: Vec<ApprovalItem>) -> InteractionCard {
        InteractionCard {
            request_id: Uuid::nil(),
            turn_id: Uuid::nil(),
            session_id: Uuid::nil(),
            device_id: Uuid::nil(),
            device_name: "Workstation".into(),
            workspace: "/workspace".into(),
            expires_at: 1900000300,
            context: InteractionContext::Approval { items },
        }
    }

    #[test]
    fn write_body_stays_in_portal_and_batch_is_explicit() {
        let item = ApprovalItem {
            tool_call_id: "call".into(),
            tool_name: "Write".into(),
            tool_input: json!({"file_path":"notes.txt","content":"private draft".repeat(1000)}),
        };
        let card = card(vec![item; 5]);
        for markdown in [false, true] {
            let summary = text(&card, "https://agent.example.test", markdown);
            assert!(!summary.contains("private draft"));
            assert!(summary.contains("13000 字符"));
            assert!(summary.contains("确认 5 项操作"));
            assert!(summary.contains("另有 2 项操作"));
            assert!(summary.contains("/?approval=00000000-0000-0000-0000-000000000000"));
            assert!(summary.chars().count() < 500);
        }
    }

    #[test]
    fn unicode_preview_is_bounded_and_cannot_inject_markdown_or_lines() {
        let item = ApprovalItem {
            tool_call_id: "call".into(),
            tool_name: "Bash".into(),
            tool_input: json!({"command":format!("[click](https://untrusted.test)\n{}", "界".repeat(4000))}),
        };
        let summary = text(&card(vec![item]), "https://agent.example.test", true);
        assert!(summary.contains("\\[click\\]\\(https://untrusted.test\\)"));
        assert!(summary.contains('…'));
        assert_eq!(summary.lines().count(), 4);
        assert!(summary.chars().count() < 400);
    }
}
