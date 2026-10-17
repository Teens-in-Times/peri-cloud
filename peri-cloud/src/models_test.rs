use super::*;
use axum::{extract::State, http::HeaderMap, routing::post, Json, Router};
use parking_lot::Mutex;
use peri_model::{ModelMessage, ModelRequest};
use serde_json::{json, Value};
use std::future::IntoFuture;
use tokio_util::sync::CancellationToken;

type ObservedRequests = Arc<Mutex<Vec<(String, String)>>>;

#[tokio::test]
async fn users_resolve_their_own_model_and_key_and_keys_are_never_returned() {
    let root = tempfile::tempdir().unwrap();
    let journal = Arc::new(CloudJournal::open(root.path()).await.unwrap());
    let identity =
        crate::identity::IdentityService::new(&journal, "fixture-bootstrap-32-character-secret")
            .await
            .unwrap();
    let first = identity
        .initialize(
            "fixture-bootstrap-32-character-secret",
            "owner",
            "fixture-password-12".into(),
            "Owner",
        )
        .await
        .unwrap();
    let login = identity
        .login("owner", "fixture-password-12".into())
        .await
        .unwrap();
    let actor = identity
        .authenticate_browser(&login.session_token)
        .await
        .unwrap();
    let invite = identity.create_invite(&actor).await.unwrap();
    let second = identity
        .register_invited(&invite.code, "guest", "fixture-password-12".into(), "Guest")
        .await
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    async fn serve(
        State(seen): State<ObservedRequests>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> ([(axum::http::HeaderName, &'static str); 1], String) {
        seen.lock().push((
            headers["authorization"].to_str().unwrap().into(),
            body["model"].as_str().unwrap().into(),
        ));
        let delta = json!({"choices":[{"delta":{"role":"assistant","content":"done"},"finish_reason":"stop"}]});
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            format!("data: {delta}\n\ndata: [DONE]\n\n"),
        )
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    async fn native(
        State(seen): State<ObservedRequests>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> ([(axum::http::HeaderName, &'static str); 1], String) {
        seen.lock().push((
            headers["x-api-key"].to_str().unwrap().into(),
            body["model"].as_str().unwrap().into(),
        ));
        ([(axum::http::header::CONTENT_TYPE,"text/event-stream")],concat!(
            "event: message_start\ndata: {\"message\":{\"id\":\"fixture\",\"usage\":{\"input_tokens\":1}}}\n\n",
            "event: message_delta\ndata: {\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
            "event: message_stop\ndata: {}\n\n").to_owned())
    }
    let base: Url = format!("http://{}/v1", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let stop = CancellationToken::new();
    let stopped = stop.clone();
    let server = tokio::spawn(
        axum::serve(
            listener,
            Router::new()
                .route("/v1/chat/completions", post(serve))
                .route("/v1/messages", post(native))
                .with_state(seen.clone()),
        )
        .with_graceful_shutdown(stopped.cancelled_owned())
        .into_future(),
    );
    let models = AccountModels::new(
        journal.clone(),
        vec![ModelProvider {
            id: "fixture".into(),
            name: "Fixture".into(),
            api_base: base.clone(),
            protocol: ModelProtocol::default(),
        }],
        "default".into(),
        128,
    )
    .unwrap();
    for (principal, model, key) in [
        (first.id, "owner-model", "OWNER_FIXTURE_KEY"),
        (second.id, "guest-model", "GUEST_FIXTURE_KEY"),
    ] {
        models
            .save(
                principal,
                ModelUpdate {
                    provider: "fixture".into(),
                    model: model.into(),
                    api_key: Some(key.into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let response = serde_json::to_string(&models.choice(principal).await.unwrap()).unwrap();
        assert!(!response.contains(key));
        let fallback = Arc::new(OpenAiModel::new(OpenAiConfig::new(
            base.clone(),
            "FALLBACK_KEY",
            "fallback",
        )));
        let model = models.resolve(principal, None, fallback).await.unwrap();
        model
            .complete(
                ModelRequest::new(vec![ModelMessage::user_text("hello")]),
                CancellationToken::new(),
            )
            .await
            .unwrap();
    }
    assert_eq!(
        *seen.lock(),
        vec![
            ("Bearer OWNER_FIXTURE_KEY".into(), "owner-model".into()),
            ("Bearer GUEST_FIXTURE_KEY".into(), "guest-model".into())
        ]
    );
    assert!(models
        .save(
            second.id,
            ModelUpdate {
                provider: "unknown".into(),
                model: "x".into(),
                api_key: Some("x".into()),
                ..Default::default()
            }
        )
        .await
        .is_err());
    models
        .save(
            second.id,
            ModelUpdate {
                provider: "fixture".into(),
                model: "changed".into(),
                api_key: None,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        models.choice(second.id).await.unwrap().unwrap().model,
        "changed"
    );
    models.reset(second.id).await.unwrap();
    assert!(models.choice(second.id).await.unwrap().is_none());
    assert_eq!(
        models.choice(first.id).await.unwrap().unwrap().model,
        "owner-model"
    );
    let mut native_base = base.clone();
    native_base.set_path("/");
    for (protocol, url, key) in [
        (ModelProtocol::Openai, base, "CUSTOM_OPENAI"),
        (
            ModelProtocol::Anthropic,
            native_base.clone(),
            "CUSTOM_ANTHROPIC",
        ),
    ] {
        models
            .save(
                first.id,
                ModelUpdate {
                    provider: "@custom".into(),
                    model: "custom-model".into(),
                    api_key: Some(key.into()),
                    api_base: Some(url),
                    protocol: Some(protocol),
                },
            )
            .await
            .unwrap();
        let model = models
            .resolve(
                first.id,
                None,
                Arc::new(OpenAiModel::new(OpenAiConfig::new(
                    native_base.clone(),
                    "unused",
                    "unused",
                ))),
            )
            .await
            .unwrap();
        model
            .complete(
                ModelRequest::new(vec![ModelMessage::user_text("fixture")]),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(seen.lock().last().unwrap().0.contains(key));
        assert!(
            !serde_json::to_string(&models.choice(first.id).await.unwrap())
                .unwrap()
                .contains(key)
        );
    }
    let mut other = native_base;
    other.set_path("/different");
    assert!(
        models
            .save(
                first.id,
                ModelUpdate {
                    provider: "@custom".into(),
                    model: "custom-model".into(),
                    api_base: Some(other),
                    ..Default::default()
                }
            )
            .await
            .is_err(),
        "changing endpoint must never silently forward the existing key"
    );
    stop.cancel();
    server.await.unwrap().unwrap();
    journal.close().await;
}
