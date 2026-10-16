use super::*;
use crate::gateway::{ChannelRoute, DeliveryBody, ReplyKind};
use crate::identity::ChannelIdentity;

fn delivery() -> Delivery {
    Delivery {
        delivery_id: Uuid::new_v4(),
        route: ChannelRoute {
            identity: ChannelIdentity {
                adapter_instance_id: "personal".into(),
                external_user_id: "67890".into(),
            },
            conversation_id: "group:11111".into(),
        },
        in_reply_to: "group:11111:-42".into(),
        reply_kind: ReplyKind::Message,
        message_sequence: 1,
        body: DeliveryBody::Notice {
            text: "literal [CQ:at,qq=all]".into(),
        },
    }
}

#[tokio::test]
async fn ack_requires_confirmed_message_id_and_never_retries_unknown_outcomes() {
    for (response, expected) in [
        (
            json!({"status":"ok","retcode":0,"data":{"message_id":-99}}),
            "ok",
        ),
        (json!({"status":"failed","retcode":1400}), "rejected"),
        (json!({"status":"failed","retcode":1200}), "unknown"),
        (json!({"status":"async","retcode":1}), "unknown"),
        (json!({"status":"ok","retcode":0,"data":{}}), "unknown"),
        (Value::Null, "unknown"),
    ] {
        let adapter =
            OneBotAdapter::new("personal".into(), "12345".into(), "a".repeat(32)).unwrap();
        let (requests, mut calls) = mpsc::channel(32);
        *adapter.connection.lock() = Some(Connection {
            id: Uuid::new_v4(),
            requests,
        });
        let sending = adapter.clone();
        let send = tokio::spawn(async move { sending.deliver(&delivery()).await });
        let call = calls.recv().await.unwrap();
        assert_eq!(call.action, "send_group_msg");
        assert_eq!(call.params["group_id"], 11111);
        assert_eq!(call.params["message"][0]["data"]["id"], "-42");
        assert_eq!(call.params["message"][1]["data"]["qq"], "67890");
        assert_eq!(
            call.params["message"][2],
            json!({"type":"text","data":{"text":"literal [CQ:at,qq=all]"}})
        );
        if response.is_null() {
            drop(call.response);
        } else {
            call.response.send(response).unwrap();
        }
        let result = send.await.unwrap();
        match expected {
            "ok" => assert!(result.is_ok()),
            "rejected" => assert!(matches!(result, Err(DeliveryError::Rejected))),
            _ => assert!(matches!(result, Err(DeliveryError::Unconfirmed))),
        }
        assert!(
            calls.try_recv().is_err(),
            "unknown sends must never be retried"
        );
        adapter.connection.lock().take();
        assert!(matches!(
            adapter.deliver(&delivery()).await,
            Err(DeliveryError::Rejected)
        ));
    }
}

#[test]
fn credential_validation_does_not_accept_empty_or_injectable_tokens() {
    for token in [
        String::new(),
        "a".repeat(31),
        format!("{}\nsecret", "a".repeat(32)),
    ] {
        assert!(OneBotAdapter::new("personal".into(), "12345".into(), token).is_err());
    }
}
