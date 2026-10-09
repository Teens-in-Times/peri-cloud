use super::*;
use peri_acp_types::messages::{ContentBlock, MessageContent};

#[test]
fn ordinary_chat_projects_only_assistant_text() {
    let messages = [
        BaseMessage::human("user input"),
        BaseMessage::system("internal instruction"),
        BaseMessage::ai_from_blocks(vec![
            ContentBlock::Reasoning {
                text: "private thought".into(),
                signature: None,
            },
            ContentBlock::Text {
                text: "我先检查一下。".into(),
            },
            ContentBlock::ToolUse {
                id: "tool-call".into(),
                name: "Read".into(),
                input: serde_json::json!({"file_path":"secret-path"}),
            },
        ]),
        BaseMessage::tool_result("tool-call", "private tool output"),
        BaseMessage::ai(MessageContent::Text("已完成。".into())),
    ];
    let payloads: Vec<_> = messages
        .into_iter()
        .map(PersistedPayload::Message)
        .collect();
    let visible = replies(&payloads);
    assert_eq!(
        visible
            .iter()
            .map(|reply| reply.text.as_str())
            .collect::<Vec<_>>(),
        ["我先检查一下。", "已完成。"]
    );
    let json = serde_json::to_string(&visible).unwrap();
    for hidden in [
        "private thought",
        "private tool output",
        "secret-path",
        "tool-call",
        "internal instruction",
        "user input",
    ] {
        assert!(!json.contains(hidden));
    }
}
