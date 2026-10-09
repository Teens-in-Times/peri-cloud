use super::*;
use crate::gateway::{ChannelRoute, InteractionCard};
use crate::identity::ChannelIdentity;
use peri_acp_types::interaction::{ApprovalItem, InteractionContext};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn delivery() -> Delivery {
    Delivery {
        delivery_id: Uuid::new_v4(),
        route: ChannelRoute {
            identity: ChannelIdentity {
                adapter_instance_id: "fixture-qq".into(),
                external_user_id: "owner-openid".into(),
            },
            conversation_id: "c2c:owner-openid".into(),
        },
        in_reply_to: "message-id".into(),
        message_sequence: 2,
        body: DeliveryBody::Interaction {
            card: InteractionCard {
                request_id: Uuid::new_v4(),
                turn_id: Uuid::new_v4(),
                session_id: Uuid::new_v4(),
                device_id: Uuid::new_v4(),
                device_name: "Windows PC".into(),
                workspace: "G:\\Project".into(),
                expires_at: 1900000300,
                context: InteractionContext::Approval {
                    items: vec![ApprovalItem {
                        tool_call_id: "call".into(),
                        tool_name: "Bash".into(),
                        tool_input: json!({"command":"Get-ChildItem"}),
                    }],
                },
            },
        },
    }
}

fn adapter(endpoints: QqEndpoints) -> Arc<QqAdapter> {
    QqAdapter::new(
        "fixture-qq".into(),
        "123".into(),
        "fixture-secret".into(),
        "https://agent.example.test",
        QqInteractionMode::default(),
        endpoints,
    )
    .unwrap()
}

/// [回归测试] QQ 客户端的指定用户限制会拦截合法点击，Hermes 交由后台校验审批归属。
#[test]
fn hermes_style_raw_markdown_keyboard_defers_click_authorization_to_gateway() {
    let adapter = adapter(QqEndpoints::default());
    let delivery = delivery();
    let body = adapter.body(&delivery).unwrap();
    assert_eq!(body["msg_type"], 2);
    assert!(body["markdown"]["custom_template_id"].is_null());
    assert!(body["markdown"]["content"]
        .as_str()
        .unwrap()
        .contains("Get-ChildItem"));
    assert_eq!(body["msg_id"], "message-id");
    assert_eq!(body["msg_seq"], 2);
    let buttons = body["keyboard"]["content"]["rows"][0]["buttons"]
        .as_array()
        .unwrap();
    assert_eq!(buttons.len(), 2);
    for button in buttons {
        assert_eq!(button["action"]["type"], 1);
        assert_eq!(
            button["action"]["permission"],
            json!({"type":2}),
            "点击到达后台后再校验实际点击者与审批会话"
        );
        assert_eq!(button["group_id"], "approval");
    }
    let mut reply = delivery;
    reply.body = DeliveryBody::Reply {
        reply: crate::ChatReply {
            message_id: "ai-message".into(),
            text: "普通 AI 回复".into(),
        },
    };
    assert_eq!(adapter.body(&reply).unwrap()["content"], "普通 AI 回复");
    assert!(adapter.body(&reply).unwrap().get("keyboard").is_none());
}

#[test]
fn interaction_modes_reject_unused_template_and_extra_fields() {
    for input in [
        json!({"kind":"native","markdown_template_id":"unused"}),
        json!({"kind":"web","owner":"forged"}),
    ] {
        assert!(serde_json::from_value::<QqInteractionMode>(input).is_err());
    }
    assert!(matches!(
        serde_json::from_value::<QqInteractionMode>(json!({"kind":"native"})).unwrap(),
        QqInteractionMode::Native {}
    ));
}

#[derive(Default)]
struct Script {
    requests: Mutex<Vec<Value>>,
    statuses: Mutex<std::collections::VecDeque<StatusCode>>,
}

async fn send_message(
    axum::extract::State(script): axum::extract::State<Arc<Script>>,
    axum::Json(body): axum::Json<Value>,
) -> (StatusCode, axum::Json<Value>) {
    script.requests.lock().push(body);
    let status = script.statuses.lock().pop_front().unwrap_or(StatusCode::OK);
    (status, axum::Json(json!({"id":"outgoing-id"})))
}

async fn fixture(
    statuses: Vec<StatusCode>,
) -> (
    Arc<QqAdapter>,
    Arc<Script>,
    CancellationToken,
    tokio::task::JoinHandle<()>,
) {
    let script = Arc::new(Script {
        statuses: Mutex::new(statuses.into()),
        ..Default::default()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}/", listener.local_addr().unwrap());
    let routes = axum::Router::new()
        .route(
            "/token",
            axum::routing::post(|| async {
                axum::Json(json!({"access_token":"fixture-token","expires_in":3600}))
            }),
        )
        .route(
            "/v2/users/{user}/messages",
            axum::routing::post(send_message),
        )
        .with_state(script.clone());
    let stop = CancellationToken::new();
    let signal = stop.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, routes)
            .with_graceful_shutdown(signal.cancelled_owned())
            .await
            .unwrap();
    });
    let endpoints = QqEndpoints::loopback(
        origin.parse().unwrap(),
        format!("{origin}token").parse().unwrap(),
    )
    .unwrap();
    (adapter(endpoints), script, stop, server)
}

#[tokio::test]
async fn explicit_native_rejection_falls_back_once_with_same_persisted_sequence() {
    let (adapter, script, stop, server) =
        fixture(vec![StatusCode::FORBIDDEN, StatusCode::OK]).await;
    adapter.deliver(&delivery()).await.unwrap();
    adapter.deliver(&delivery()).await.unwrap();
    let requests = script.requests.lock().clone();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].get("keyboard").is_some());
    assert!(requests[1].get("keyboard").is_none());
    assert!(requests[1]["content"]
        .as_str()
        .unwrap()
        .contains("/?approval="));
    assert_eq!(requests[0]["msg_seq"], requests[1]["msg_seq"]);
    assert_eq!(requests[0]["msg_id"], requests[1]["msg_id"]);
    assert!(requests[2].get("keyboard").is_none());
    stop.cancel();
    server.await.unwrap();
}

#[tokio::test]
async fn unknown_native_delivery_does_not_send_a_fallback_duplicate() {
    let (adapter, script, stop, server) = fixture(vec![StatusCode::BAD_GATEWAY]).await;
    assert!(matches!(
        adapter.deliver(&delivery()).await,
        Err(DeliveryError::Unconfirmed)
    ));
    assert_eq!(script.requests.lock().len(), 1);
    stop.cancel();
    server.await.unwrap();
}
