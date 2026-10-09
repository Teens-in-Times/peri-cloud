use super::*;
use serde_json::json;

fn interaction() -> Value {
    json!({"id":"interaction-42","chat_type":2,"user_openid":"owner-openid","data":{"type":11,"resolved":{"button_data":"peri:allow:00000000-0000-0000-0000-000000000042","user_id":"forged-guild-user"}}})
}

#[test]
fn hermes_nested_button_type_routes_to_actual_c2c_sender() {
    let Some(QqInput::Interaction {
        route,
        request,
        action,
        ..
    }) = normalize("qq", "INTERACTION_CREATE", interaction()).unwrap()
    else {
        panic!("button event was discarded");
    };
    assert_eq!(route.identity.external_user_id, "owner-openid");
    assert_eq!(route.conversation_id, "c2c:owner-openid");
    assert_eq!(request.to_string(), "00000000-0000-0000-0000-000000000042");
    assert!(matches!(action, InteractionAction::AllowOnce {}));
}

#[test]
fn duplicated_documented_type_is_accepted_but_conflicting_type_is_rejected() {
    let mut data = interaction();
    data["type"] = json!(11);
    assert!(matches!(
        normalize("qq", "INTERACTION_CREATE", data.clone()).unwrap(),
        Some(QqInput::Interaction { .. })
    ));
    data["type"] = json!(12);
    assert!(matches!(
        normalize("qq", "INTERACTION_CREATE", data),
        Err(QqError::Event)
    ));
}

#[test]
fn missing_c2c_sender_never_uses_resolved_guild_user() {
    let mut data = interaction();
    data.as_object_mut().unwrap().remove("user_openid");
    assert!(matches!(
        normalize("qq", "INTERACTION_CREATE", data),
        Err(QqError::Event)
    ));
}

#[test]
fn group_button_uses_observed_member_and_group() {
    let mut data = interaction();
    data["chat_type"] = json!(1);
    data["group_member_openid"] = json!("member-openid");
    data["group_openid"] = json!("group-openid");
    let Some(QqInput::Interaction { route, .. }) =
        normalize("qq", "INTERACTION_CREATE", data).unwrap()
    else {
        panic!("missing interaction");
    };
    assert_eq!(route.identity.external_user_id, "member-openid");
    assert_eq!(route.conversation_id, "group:group-openid");
}

#[test]
fn actual_qq_message_fields_map_to_private_sender_route() {
    let message =
        json!({"id":"message-42","content":" /帮助 ","author":{"user_openid":"owner-openid"}});
    let Some(QqInput::Message(message)) = normalize("qq", "C2C_MESSAGE_CREATE", message).unwrap()
    else {
        panic!("missing message");
    };
    assert_eq!(message.route.conversation_id, "c2c:owner-openid");
    assert_eq!(message.text, " /帮助 ");
    let forged = json!({"id":"message-43","content":"/帮助","author":{"id":"owner-openid"}});
    assert!(matches!(
        normalize("qq", "C2C_MESSAGE_CREATE", forged),
        Err(QqError::Event)
    ));
}
