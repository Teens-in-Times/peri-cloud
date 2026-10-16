use super::*;
use serde_json::json;

fn event(message: Value) -> Value {
    json!({"post_type":"message","message_type":"group","sub_type":"normal","self_id":12345,
        "user_id":67890,"group_id":11111,"message_id":-42,"message":message})
}

#[test]
fn groups_require_explicit_address_and_preserve_exact_route() {
    let plain = |text: &str| json!([{"type":"text","data":{"text":text}}]);
    assert!(normalize("personal", "12345", &event(plain("群内闲聊"))).is_none());
    assert!(normalize("personal", "12345", &event(plain("/别的机器人命令"))).is_none());
    assert!(normalize("personal", "12345", &event(plain("2"))).is_none());
    let input = normalize(
        "personal",
        "12345",
        &event(json!([
            {"type":"at","data":{"qq":"12345"}}, {"type":"text","data":{"text":" 看资源 "}}
        ])),
    )
    .unwrap();
    assert_eq!(input.text, "看资源");
    assert_eq!(input.route.identity.external_user_id, "67890");
    assert_eq!(input.route.identity.adapter_instance_id, "personal");
    assert_eq!(input.route.conversation_id, "group:11111");
    assert_eq!(input.event_id, "group:11111:-42");
    assert!(normalize("personal", "12345", &event(plain("/菜单"))).is_some());
}

#[test]
fn self_events_anonymous_wrong_account_and_partial_multimedia_are_ignored() {
    let base = event(json!([{"type":"text","data":{"text":"/菜单"}}]));
    for (field, value) in [
        ("user_id", json!(12345)),
        ("self_id", json!(55555)),
        ("anonymous", json!({"id":1})),
        ("post_type", json!("message_sent")),
        ("message", json!("[CQ:at,qq=12345] /菜单")),
        (
            "message",
            json!([{"type":"text","data":{"text":"/菜单"}},{"type":"image","data":{"file":"private.png"}}]),
        ),
    ] {
        let mut input = base.clone();
        input[field] = value;
        assert!(normalize("personal", "12345", &input).is_none(), "{field}");
    }
    let mut private = base;
    private["message_type"] = json!("private");
    private["sub_type"] = json!("friend");
    private["message"] = json!("查看状态");
    assert_eq!(
        normalize("personal", "12345", &private)
            .unwrap()
            .route
            .conversation_id,
        "private:67890"
    );
    private["sub_type"] = json!("group");
    assert!(normalize("personal", "12345", &private).is_none());
}

#[test]
fn numeric_quote_requires_original_bot_authorship_and_real_snapshot_id() {
    let candidate =
        event(json!([{"type":"reply","data":{"id":"-99"}},{"type":"text","data":{"text":"2"}}]));
    assert!(normalize("personal", "12345", &candidate).is_none());
    assert_eq!(
        parse("personal", "12345", &candidate).unwrap().quoted,
        Some(-99)
    );
    let snapshot = Uuid::new_v4();
    let mut response = json!({"status":"ok","retcode":0,"data":{"sender":{"user_id":12345},"message":[
        {"type":"reply","data":{"id":"-42"}}, {"type":"at","data":{"qq":"67890"}},
        {"type":"text","data":{"text":format!("菜单全文\n菜单引用：{snapshot}")}}
    ]}});
    assert_eq!(quoted_menu(&response, "12345"), Some(snapshot));
    response["data"]["sender"]["user_id"] = json!(67890);
    assert!(quoted_menu(&response, "12345").is_none());
}
