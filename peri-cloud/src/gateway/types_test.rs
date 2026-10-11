use super::*;
use serde_json::json;

#[test]
fn old_message_fingerprints_remain_stable_and_events_have_distinct_dedup_keys() {
    let old = r#"{"route":{"identity":{"adapter_instance_id":"qq","external_user_id":"user"},"conversation_id":"c2c:user"},"event_id":"id-42","text":"菜单"}"#;
    let message: InboundMessage = serde_json::from_str(old).unwrap();
    assert_eq!(message.reply_kind, ReplyKind::Message);
    assert_eq!(message.fingerprint().unwrap(), digest(old.as_bytes()));
    assert_eq!(serde_json::to_string(&message).unwrap(), old);
    let mut event = message.clone();
    event.reply_kind = ReplyKind::Event;
    assert_ne!(event.key().unwrap(), message.key().unwrap());
    assert_ne!(event.fingerprint().unwrap(), message.fingerprint().unwrap());
}

#[test]
fn old_outbox_records_default_to_message_references() {
    let record = json!({"delivery_id":Uuid::nil(),"route":{"identity":{"adapter_instance_id":"qq","external_user_id":"user"},"conversation_id":"c2c:user"},
        "in_reply_to":"old-message","message_sequence":3,"body":{"kind":"notice","text":"saved"}});
    let delivery: Delivery = serde_json::from_value(record).unwrap();
    assert_eq!(delivery.reply_kind, ReplyKind::Message);
    assert_eq!(delivery.message_sequence, 3);
    assert_eq!(delivery.in_reply_to, "old-message");
}

#[test]
fn old_device_metadata_does_not_claim_a_server_local_instance() {
    let record = json!({"id":Uuid::nil(),"name":"PC","platform":"windows","default_workspace":"C:\\Projects","connection_id":null,"revoked":false});
    let device: peri_acp_types::device_enrollment::DeviceRecord =
        serde_json::from_value(record.clone()).unwrap();
    assert!(!device.is_local);
    assert_eq!(serde_json::to_value(device).unwrap(), record);
}
