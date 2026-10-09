use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{security, Portal, PortalError};
use crate::identity::{DeviceRecord, IdentityError, NativeAuthorization, RegistrationReceipt};

pub(super) async fn consent_page(
    RawQuery(query): RawQuery,
) -> Result<Html<&'static str>, PortalError> {
    let query = query.ok_or(PortalError::Invalid)?;
    let mut fields = serde_json::Map::new();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if fields
            .insert(key.into_owned(), Value::String(value.into_owned()))
            .is_some()
        {
            return Err(PortalError::Invalid);
        }
    }
    if fields.remove("response_type") != Some(Value::String("code".into())) {
        return Err(PortalError::Invalid);
    }
    let authorization: NativeAuthorization =
        serde_json::from_value(Value::Object(fields)).map_err(|_| PortalError::Invalid)?;
    authorization.validate().map_err(|_| PortalError::Invalid)?;
    Ok(Html(include_str!("assets/index.html")))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Consent {
    authorization: NativeAuthorization,
    allow: bool,
}

pub(super) async fn consent(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Json(input): Json<Consent>,
) -> Result<Json<Value>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    input
        .authorization
        .validate()
        .map_err(|_| PortalError::Invalid)?;
    let redirect = if input.allow {
        portal
            .identity
            .authorize_native(&actor, input.authorization)
            .await?
    } else {
        let mut redirect =
            url::Url::parse(&input.authorization.redirect_uri).map_err(|_| PortalError::Invalid)?;
        redirect
            .query_pairs_mut()
            .append_pair("error", "access_denied")
            .append_pair("state", &input.authorization.state);
        redirect.to_string()
    };
    // Explicit consent is a browser POST; JS performs the local callback after
    // approval. No redirect to a caller-controlled non-loopback destination.
    Ok(Json(json!({"redirect_uri": redirect})))
}

pub(super) async fn token(
    State(portal): State<Portal>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let form = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case("application/x-www-form-urlencoded")
        });
    if !form {
        return oauth_error("invalid_request", StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    let mut input = HashMap::new();
    for (key, value) in url::form_urlencoded::parse(&body) {
        if input.insert(key.into_owned(), value.into_owned()).is_some() {
            return oauth_error("invalid_request", StatusCode::BAD_REQUEST);
        }
    }
    let result = match input.get("grant_type").map(String::as_str) {
        Some("authorization_code") => {
            portal
                .identity
                .exchange_code(
                    field(&input, "client_id"),
                    field(&input, "code"),
                    field(&input, "code_verifier"),
                    field(&input, "redirect_uri"),
                )
                .await
        }
        Some("refresh_token") => {
            portal
                .identity
                .refresh_native(field(&input, "client_id"), field(&input, "refresh_token"))
                .await
        }
        _ => return oauth_error("unsupported_grant_type", StatusCode::BAD_REQUEST),
    };
    match result {
        Ok(tokens) => Json(tokens).into_response(),
        Err(IdentityError::RateLimited) => {
            oauth_error("temporarily_unavailable", StatusCode::TOO_MANY_REQUESTS)
        }
        Err(IdentityError::Database(_) | IdentityError::Encoding(_) | IdentityError::Crypto) => {
            tracing::error!("native authorization state is unavailable");
            oauth_error("temporarily_unavailable", StatusCode::SERVICE_UNAVAILABLE)
        }
        Err(_) => oauth_error("invalid_grant", StatusCode::BAD_REQUEST),
    }
}

fn field<'a>(input: &'a HashMap<String, String>, name: &str) -> &'a str {
    input.get(name).map(String::as_str).unwrap_or_default()
}

fn oauth_error(code: &str, status: StatusCode) -> Response {
    (status, Json(json!({"error": code}))).into_response()
}

pub(super) async fn register_device(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Json(device): Json<DeviceRecord>,
) -> Result<Json<RegistrationReceipt>, PortalError> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| value.len() == 64)
        .ok_or(IdentityError::Unauthorized)?;
    let actor = portal.identity.authenticate_native(bearer).await?;
    Ok(Json(portal.identity.register_device(&actor, device).await?))
}
