use super::*;
use axum::{
    extract::{Request, State},
    http::{header::CONTENT_TYPE, StatusCode},
    response::IntoResponse,
    Json, Router,
};
use parking_lot::Mutex;
use peri_model::{ModelMessage, ModelRequest};
use serde_json::{json, Value};
use std::future::IntoFuture;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Script {
    calls: Mutex<Vec<(String, String, String, Value)>>,
    status: Mutex<i64>,
    catalog_offline: std::sync::atomic::AtomicBool,
    fail_write_response: std::sync::atomic::AtomicBool,
    pages: Mutex<Vec<String>>,
}
async fn respond(State(script): State<Arc<Script>>, request: Request) -> axum::response::Response {
    let method = request.method().to_string();
    let path = request.uri().path().to_owned();
    if path == "/api/channel/" {
        script
            .pages
            .lock()
            .push(request.uri().query().unwrap().to_owned());
    }
    let key = request
        .headers()
        .get("authorization")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let bytes = axum::body::to_bytes(request.into_body(), 65536)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    script
        .calls
        .lock()
        .push((method.clone(), path.clone(), key.clone(), body));
    assert_eq!(
        key,
        if path.starts_with("/v1/") {
            "Bearer INFERENCE_FIXTURE"
        } else {
            "Bearer MANAGEMENT_FIXTURE"
        }
    );
    let channel = json!({"id":7,"name":"Fixture","type":1,"status":*script.status.lock(),"models":"model-a,model-b","group":"default","priority":1,"weight":1,"base_url":"https://api.example.test","model_mapping":"{}","test_time":0,"response_time":0,"key":"UPSTREAM_MUST_NOT_ESCAPE","secret_extra":"OPAQUE_SECRET"});
    let value = match (method.as_str(), path.as_str()) {
        ("GET", "/v1/models") => {
            if script
                .catalog_offline
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            json!({"data":[{"id":"model-a"},{"id":"model-b"}]})
        }
        ("GET", "/api/channel/") => json!({"success":true,"data":{"items":[channel],"total":1}}),
        ("GET", "/api/channel/7") => json!({"success":true,"data":channel}),
        ("GET", "/api/group/") => json!({"success":true,"data":["default"]}),
        ("POST", "/api/channel/7/status") => {
            *script.status.lock() = script.calls.lock().last().unwrap().3["status"]
                .as_i64()
                .unwrap();
            if script
                .fail_write_response
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            json!({"success":true,"data":true})
        }
        ("POST", "/v1/chat/completions") => {
            let delta = json!({"choices":[{"delta":{"role":"assistant","content":"done"},"finish_reason":"stop"}]});
            return (
                [(CONTENT_TYPE, "text/event-stream")],
                format!("data: {delta}\n\ndata: [DONE]\n\n"),
            )
                .into_response();
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    Json(value).into_response()
}
#[tokio::test]
async fn management_is_deterministic_redacted_owned_versioned_and_idempotent() {
    let root = tempfile::tempdir().unwrap();
    let journal = Arc::new(CloudJournal::open(root.path()).await.unwrap());
    let identity = IdentityService::new(&journal, "fixture-aggregation-bootstrap-token-32")
        .await
        .unwrap();
    let owner = identity
        .initialize(
            "fixture-aggregation-bootstrap-token-32",
            "owner",
            "fixture-password-long".into(),
            "Owner",
        )
        .await
        .unwrap();
    let login = identity
        .login("owner", "fixture-password-long".into())
        .await
        .unwrap();
    let actor = identity
        .authenticate_browser(&login.session_token)
        .await
        .unwrap();
    let invite = identity.create_invite(&actor).await.unwrap();
    identity
        .register_invited(
            &invite.code,
            "guest",
            "fixture-password-long".into(),
            "Guest",
        )
        .await
        .unwrap();
    let guest_login = identity
        .login("guest", "fixture-password-long".into())
        .await
        .unwrap();
    let guest = identity
        .authenticate_browser(&guest_login.session_token)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let script = Arc::new(Script::default());
    *script.status.lock() = 1;
    let config = AggregationConfig {
        origin: format!("http://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap(),
        management_key_file: "management".into(),
        inference_key_file: "inference".into(),
    };
    let stop = CancellationToken::new();
    let stopped = stop.clone();
    let server = tokio::spawn(
        axum::serve(
            listener,
            Router::new().fallback(respond).with_state(script.clone()),
        )
        .with_graceful_shutdown(stopped.cancelled_owned())
        .into_future(),
    );
    let aggregate = Aggregation::new(
        &config,
        "MANAGEMENT_FIXTURE".into(),
        "INFERENCE_FIXTURE".into(),
        identity,
        journal.clone(),
        "model-a".into(),
        128,
    )
    .unwrap();
    let owner_actor = ControlActor::Browser(&actor);
    let guest_actor = ControlActor::Browser(&guest);
    assert!(aggregate.channels(&guest_actor, 0).await.is_err());
    assert!(
        script.calls.lock().is_empty(),
        "authorization before external call"
    );
    let page = aggregate.channels(&owner_actor, 0).await.unwrap();
    aggregate.channels(&owner_actor, 1).await.unwrap();
    assert_eq!(
        *script.pages.lock(),
        vec!["p=1&page_size=30", "p=2&page_size=30"],
        "New API uses one-based pagination"
    );
    let encoded = serde_json::to_string(&page).unwrap();
    for secret in [
        "UPSTREAM_MUST_NOT_ESCAPE",
        "OPAQUE_SECRET",
        "MANAGEMENT_FIXTURE",
        "INFERENCE_FIXTURE",
    ] {
        assert!(!encoded.contains(secret));
    }
    let revision = page.items[0].revision.clone();
    let command = ChannelCommand {
        request_id: Uuid::new_v4(),
        confirmed: true,
        action: ChannelAction::Status {
            id: 7,
            revision: revision.clone(),
            enabled: false,
        },
    };
    let duplicate = serde_json::from_value(serde_json::to_value(&command).unwrap()).unwrap();
    aggregate
        .channel_command(&owner_actor, command)
        .await
        .unwrap();
    aggregate
        .channel_command(&owner_actor, duplicate)
        .await
        .unwrap();
    assert_eq!(
        script.calls.lock().iter().filter(|c| c.0 == "POST").count(),
        1,
        "duplicate must not repeat mutation"
    );
    assert!(matches!(
        aggregate
            .channel_command(
                &owner_actor,
                ChannelCommand {
                    request_id: Uuid::new_v4(),
                    confirmed: true,
                    action: ChannelAction::Status {
                        id: 7,
                        revision,
                        enabled: true
                    }
                }
            )
            .await,
        Err(AggregateError::Conflict)
    ));
    let session = Uuid::new_v4();
    journal
        .bind(
            crate::state::FrozenSession {
                session_id: session,
                principal_id: owner.id,
                device_id: Uuid::new_v4(),
                binding: peri_acp_types::device_executor::SessionBinding {
                    session_id: Uuid::new_v4(),
                    workspace: "/".into(),
                    workspace_identity: peri_acp_types::device_executor::WorkspaceIdentity {
                        device: 1,
                        inode: 1,
                    },
                },
                system_prompt: "fixture".into(),
                context_window: 128000,
            },
            peri_acp_types::permission::PermissionMode::Default,
        )
        .await
        .unwrap();
    assert!(matches!(
        aggregate.selection(&guest_actor, Some(session)).await,
        Err(AggregateError::Ownership)
    ));
    aggregate
        .choose_model(
            &owner_actor,
            Some(session),
            ModelSelection {
                model: Some("model-b".into()),
                revision: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        aggregate.current_model(owner.id, None).await.unwrap(),
        "model-a",
        "session change must not change account default"
    );
    assert!(matches!(
        aggregate
            .choose_model(
                &owner_actor,
                Some(session),
                ModelSelection {
                    model: None,
                    revision: 0
                }
            )
            .await,
        Err(AggregateError::Conflict)
    ));
    assert!(
        script
            .calls
            .lock()
            .iter()
            .all(|c| c.1 != "/v1/chat/completions"),
        "management must not call a model"
    );
    let turn = Uuid::new_v4();
    let model = aggregate
        .model_for_turn(owner.id, Some(session), Some(turn))
        .await
        .unwrap();
    model
        .complete(
            ModelRequest::new(vec![ModelMessage::user_text("hello")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(script.calls.lock().last().unwrap().3["model"], "model-b");
    script
        .catalog_offline
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(aggregate.models(&owner_actor).await.is_err());
    let current = aggregate
        .channels(&owner_actor, 0)
        .await
        .unwrap()
        .items
        .remove(0);
    aggregate
        .choose_model(
            &owner_actor,
            Some(session),
            ModelSelection {
                model: None,
                revision: 1,
            },
        )
        .await
        .unwrap();
    let recorded: String = sqlx::query_scalar("SELECT model FROM turn_models WHERE turn_id=?")
        .bind(turn.to_string())
        .fetch_one(&journal.identity_pool())
        .await
        .unwrap();
    assert_eq!(
        recorded, "model-b",
        "later selection must not relabel a running turn"
    );
    assert_eq!(
        aggregate
            .current_model(owner.id, Some(session))
            .await
            .unwrap(),
        "model-a"
    );
    script
        .fail_write_response
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let id = Uuid::new_v4();
    let make_unknown = || ChannelCommand {
        request_id: id,
        confirmed: true,
        action: ChannelAction::Status {
            id: 7,
            revision: current.revision.clone(),
            enabled: true,
        },
    };
    assert!(matches!(
        aggregate
            .channel_command(&owner_actor, make_unknown())
            .await,
        Err(AggregateError::Unknown)
    ));
    assert!(matches!(
        aggregate
            .channel_command(&owner_actor, make_unknown())
            .await,
        Err(AggregateError::Unknown)
    ));
    assert_eq!(
        script
            .calls
            .lock()
            .iter()
            .filter(|c| c.1 == "/api/channel/7/status" && c.0 == "POST")
            .count(),
        2,
        "unknown outcome must never replay"
    );
    let audit = aggregate.audit(&owner_actor).await.unwrap().to_string();
    assert!(!audit.contains("FIXTURE"));
    stop.cancel();
    server.await.unwrap().unwrap();
}
