use super::payload;
use crate::gateway::{ChannelRoute, Delivery, DeliveryBody, ReplyKind};
use crate::identity::ChannelIdentity;
use serde_json::json;
use uuid::Uuid;

fn delivery(conversation: &str, in_reply_to: &str, reply_kind: ReplyKind) -> Delivery {
    Delivery {
        delivery_id: Uuid::new_v4(),
        route: ChannelRoute {
            identity: ChannelIdentity {
                adapter_instance_id: "personal".into(),
                external_user_id: "67890".into(),
            },
            conversation_id: conversation.into(),
        },
        in_reply_to: in_reply_to.into(),
        reply_kind,
        message_sequence: 1,
        body: DeliveryBody::Notice {
            text: "hello".into(),
        },
    }
}

#[test]
fn private_replies_are_standalone_messages() {
    let delivery = delivery("private:67890", "private:67890:-42", ReplyKind::Message);
    let (action, params) = payload(&delivery).unwrap();
    assert_eq!(action, "send_private_msg");
    assert_eq!(params["user_id"], 67890);
    assert_eq!(
        params["message"],
        json!([{"type":"text","data":{"text":"hello"}}])
    );
}

#[test]
fn group_replies_still_quote_and_address_the_sender() {
    let delivery = delivery("group:11111", "group:11111:-42", ReplyKind::Message);
    let (action, params) = payload(&delivery).unwrap();
    assert_eq!(action, "send_group_msg");
    assert_eq!(params["group_id"], 11111);
    assert_eq!(params["message"][0]["type"], "reply");
    assert_eq!(params["message"][0]["data"]["id"], "-42");
    assert_eq!(params["message"][1]["type"], "at");
    assert_eq!(params["message"][1]["data"]["qq"], "67890");
    assert_eq!(params["message"][2]["type"], "text");
}

#[test]
fn proactive_private_replies_do_not_quote() {
    let delivery = delivery("private:67890", "private:67890:-42", ReplyKind::Proactive);
    let (_, params) = payload(&delivery).unwrap();
    assert_eq!(
        params["message"],
        json!([{"type":"text","data":{"text":"hello"}}])
    );
}
