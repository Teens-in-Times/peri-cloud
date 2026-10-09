use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use ed25519_dalek::{Signature, Signer, SigningKey};
use serde_json::{json, Value};

use super::{QqAdapter, QqError, QqResult};
use crate::gateway::Gateway;

#[derive(Clone)]
struct Webhook {
    adapter: Arc<QqAdapter>,
    gateway: Arc<Gateway>,
}

/// Mount on a TLS reverse proxy with the configured public QQ callback path.
/// Platform signatures, rather than browser cookies, authenticate these events.
pub fn webhook_router(adapter: Arc<QqAdapter>, gateway: Arc<Gateway>) -> Router {
    Router::new()
        .route("/qq/events", post(event))
        .layer(DefaultBodyLimit::max(65536))
        .with_state(Webhook { adapter, gateway })
}

async fn event(
    State(state): State<Webhook>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    handle(&state, &headers, &body)
        .await
        .map(Json)
        .map_err(|error| {
            let (status, code) = match error {
                QqError::Authentication => (StatusCode::UNAUTHORIZED, "qq_signature_rejected"),
                QqError::Event | QqError::Configuration => {
                    (StatusCode::BAD_REQUEST, "qq_event_invalid")
                }
                _ => (StatusCode::SERVICE_UNAVAILABLE, "qq_admission_unavailable"),
            };
            (status, Json(json!({"error":code})))
        })
}

async fn handle(state: &Webhook, headers: &HeaderMap, body: &[u8]) -> QqResult<Value> {
    if one_header(headers, "x-bot-appid")? != state.adapter.app_id {
        return Err(QqError::Authentication);
    }
    let payload: Value = serde_json::from_slice(body).map_err(|_| QqError::Event)?;
    let key = signing_key(&state.adapter.secret)?;
    if payload["op"].as_u64() == Some(13) {
        // QQ's documented callback verification request has Appid but no
        // signature headers. Only sign the bounded challenge fields, not body.
        let token = payload["d"]["plain_token"]
            .as_str()
            .filter(|value| !value.is_empty() && value.len() <= 256)
            .ok_or(QqError::Event)?;
        let timestamp = payload["d"]["event_ts"]
            .as_str()
            .filter(|value| {
                !value.is_empty() && value.len() <= 32 && value.bytes().all(|c| c.is_ascii_digit())
            })
            .ok_or(QqError::Event)?;
        let signature = challenge_signature(&key, token, timestamp)?;
        return Ok(json!({"plain_token":token,"signature":hex(&signature.to_bytes())}));
    }
    let timestamp = one_header(headers, "x-signature-timestamp")?;
    let seconds = timestamp
        .parse::<u64>()
        .map_err(|_| QqError::Authentication)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| QqError::Authentication)?
        .as_secs();
    if seconds.abs_diff(now) > 300 {
        return Err(QqError::Authentication);
    }
    verify(
        &key,
        timestamp,
        body,
        one_header(headers, "x-signature-ed25519")?,
    )?;
    if payload["op"].as_u64() != Some(0) {
        return Err(QqError::Event);
    }
    let kind = payload["t"].as_str().ok_or(QqError::Event)?;
    state
        .adapter
        .accept(&state.gateway, kind, payload["d"].clone())
        .await?;
    Ok(json!({"op":12}))
}

fn one_header<'a>(headers: &'a HeaderMap, name: &str) -> QqResult<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values
        .next()
        .and_then(|value| value.to_str().ok())
        .ok_or(QqError::Authentication)?;
    if values.next().is_some() || value.is_empty() || value.len() > 256 {
        return Err(QqError::Authentication);
    }
    Ok(value)
}

fn signing_key(secret: &str) -> QqResult<SigningKey> {
    if secret.is_empty() {
        return Err(QqError::Configuration);
    }
    let bytes = secret.as_bytes();
    let mut seed = [0u8; 32];
    for (index, byte) in seed.iter_mut().enumerate() {
        *byte = bytes[index % bytes.len()];
    }
    Ok(SigningKey::from_bytes(&seed))
}

fn challenge_signature(key: &SigningKey, token: &str, timestamp: &str) -> QqResult<Signature> {
    // The unsigned URL challenge must not become a chosen-event signing oracle.
    // QQ supplies an opaque token; event JSON contains characters excluded here.
    if token.is_empty()
        || token.len() > 256
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    {
        return Err(QqError::Event);
    }
    Ok(key.sign(format!("{timestamp}{token}").as_bytes()))
}

fn verify(key: &SigningKey, timestamp: &str, body: &[u8], encoded: &str) -> QqResult<()> {
    if encoded.len() != 128 {
        return Err(QqError::Authentication);
    }
    let mut bytes = [0u8; 64];
    for (index, pair) in encoded.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let high = (pair[0] as char)
            .to_digit(16)
            .ok_or(QqError::Authentication)?;
        let low = (pair[1] as char)
            .to_digit(16)
            .ok_or(QqError::Authentication)?;
        bytes[index] = (high * 16 + low) as u8;
    }
    let mut signed = timestamp.as_bytes().to_vec();
    signed.extend_from_slice(body);
    key.verifying_key()
        .verify_strict(&signed, &Signature::from_bytes(&bytes))
        .map_err(|_| QqError::Authentication)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
#[path = "webhook_test.rs"]
mod tests;
