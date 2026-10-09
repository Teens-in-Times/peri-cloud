use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use super::Portal;
use crate::identity::{Authenticated, IdentityError};

#[derive(Debug, thiserror::Error)]
pub enum PortalError {
    #[error("portal requires a valid HTTPS or literal loopback origin")]
    Configuration,
    #[error("invalid portal request")]
    Invalid,
    #[error("portal identity request failed")]
    Identity(#[from] IdentityError),
    #[error("portal interaction request failed")]
    Interaction(#[from] crate::gateway::GatewayError),
}

impl IntoResponse for PortalError {
    fn into_response(self) -> Response {
        let (status, code) = match self {
            Self::Interaction(crate::gateway::GatewayError::Invalid) => {
                (StatusCode::BAD_REQUEST, "invalid_workspace")
            }
            Self::Interaction(
                crate::gateway::GatewayError::Busy
                | crate::gateway::GatewayError::State(crate::state::StateError::Busy),
            ) => (StatusCode::CONFLICT, "workspace_busy"),
            Self::Interaction(crate::gateway::GatewayError::Connection) => {
                (StatusCode::BAD_GATEWAY, "workspace_unavailable")
            }
            Self::Interaction(crate::gateway::GatewayError::StaleInteraction) => {
                (StatusCode::CONFLICT, "stale_request")
            }
            Self::Interaction(crate::gateway::GatewayError::Identity(
                IdentityError::Unauthorized,
            )) => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::Interaction(crate::gateway::GatewayError::Identity(IdentityError::Forbidden)) => {
                (StatusCode::FORBIDDEN, "forbidden")
            }
            Self::Identity(IdentityError::Unauthorized) => {
                (StatusCode::UNAUTHORIZED, "unauthorized")
            }
            Self::Identity(IdentityError::Forbidden) => (StatusCode::FORBIDDEN, "forbidden"),
            Self::Identity(IdentityError::RateLimited) => {
                (StatusCode::TOO_MANY_REQUESTS, "rate_limited")
            }
            Self::Identity(IdentityError::Stale) => (StatusCode::CONFLICT, "stale_request"),
            Self::Identity(
                IdentityError::AlreadyInitialized | IdentityError::OwnershipConflict,
            ) => (StatusCode::CONFLICT, "identity_conflict"),
            Self::Configuration | Self::Invalid | Self::Identity(IdentityError::Invalid) => {
                (StatusCode::BAD_REQUEST, "invalid_request")
            }
            _ => {
                tracing::error!("portal identity state is unavailable");
                (StatusCode::SERVICE_UNAVAILABLE, "identity_unavailable")
            }
        };
        (status, Json(json!({"error": code}))).into_response()
    }
}

pub(super) async fn guard(State(portal): State<Portal>, request: Request, next: Next) -> Response {
    let native =
        request.uri().path() == "/oauth/token" || request.uri().path().starts_with("/api/native/");
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .or_else(|| {
            request
                .uri()
                .authority()
                .map(|authority| authority.as_str())
        });
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    let mutation = !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    let ambiguous = ["host", "origin", "authorization", "x-peri-csrf"]
        .iter()
        .any(|name| request.headers().get_all(*name).iter().count() > 1);
    let invalid_host = ambiguous || host != Some(portal.config.authority.as_str());
    let invalid_origin = (mutation && !native && origin != Some(portal.config.origin.as_str()))
        || origin.is_some_and(|value| value != portal.config.origin);
    let too_long = request.uri().to_string().len() > 8192;
    let mut response = if invalid_host || invalid_origin {
        (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "origin_rejected"})),
        )
            .into_response()
    } else if too_long {
        (
            StatusCode::URI_TOO_LONG,
            Json(json!({"error": "invalid_request"})),
        )
            .into_response()
    } else {
        next.run(request).await
    };
    // Framework extraction errors must never echo submitted identity secrets.
    if response.status().is_client_error()
        && response
            .headers()
            .get(header::CONTENT_TYPE)
            .is_none_or(|value| value != "application/json")
    {
        response = (response.status(), Json(json!({"error": "invalid_request"}))).into_response();
    }
    for (name, value) in [
        ("cache-control", "no-store"),
        ("pragma", "no-cache"),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
        ("referrer-policy", "no-referrer"),
        ("content-security-policy", "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"),
    ] {
        response.headers_mut().insert(name, HeaderValue::from_static(value));
    }
    response
}

pub(super) async fn browser(
    portal: &Portal,
    headers: &HeaderMap,
    mutation: bool,
) -> Result<Authenticated, PortalError> {
    let token = cookie(headers, portal.config.session_cookie())?;
    let actor = portal.identity.authenticate_browser(token).await?;
    if mutation {
        let csrf = headers
            .get("x-peri-csrf")
            .and_then(|value| value.to_str().ok())
            .ok_or(IdentityError::Unauthorized)?;
        portal.identity.verify_csrf(&actor, csrf).await?;
    }
    Ok(actor)
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, PortalError> {
    let mut found = None;
    for header in headers.get_all(header::COOKIE) {
        for pair in header
            .to_str()
            .map_err(|_| PortalError::Invalid)?
            .split(';')
        {
            if let Some((key, value)) = pair.trim().split_once('=') {
                if key == name {
                    if found.is_some()
                        || value.len() != 64
                        || !value.bytes().all(|c| c.is_ascii_hexdigit())
                    {
                        return Err(IdentityError::Unauthorized.into());
                    }
                    found = Some(value);
                }
            }
        }
    }
    found.ok_or_else(|| IdentityError::Unauthorized.into())
}

pub(super) fn set_cookies(portal: &Portal, session: &str, csrf: &str, max_age: u64) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let secure = if portal.config.secure { "; Secure" } else { "" };
    for (name, value, http_only) in [
        (portal.config.session_cookie(), session, "; HttpOnly"),
        (portal.config.csrf_cookie(), csrf, ""),
    ] {
        let value = format!(
            "{name}={value}; Path=/; Max-Age={max_age}; SameSite=Strict{secure}{http_only}"
        );
        headers.append(
            header::SET_COOKIE,
            HeaderValue::from_str(&value).expect("generated cookie contains only safe token bytes"),
        );
    }
    headers
}
