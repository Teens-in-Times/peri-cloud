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
    #[error("memory request failed")]
    Memory(#[from] crate::memory::MemoryError),
    #[error("aggregate control failed")]
    Aggregate(#[from] crate::aggregation::AggregateError),
    #[error("portal requires a valid HTTPS or literal loopback origin")]
    Configuration,
    #[error("invalid portal request")]
    Invalid,
    #[error("portal identity request failed")]
    Identity(#[from] IdentityError),
    #[error("portal interaction request failed")]
    Interaction(#[from] crate::gateway::GatewayError),
    #[error("personal data request failed")]
    State(#[from] crate::state::StateError),
    #[error("personal model request failed")]
    Model(#[from] crate::models::ModelConfigError),
}

impl IntoResponse for PortalError {
    fn into_response(self) -> Response {
        let (status, code) = match self {
            Self::Memory(crate::memory::MemoryError::Forbidden) => {
                (StatusCode::FORBIDDEN, "memory_scope_forbidden")
            }
            Self::Interaction(crate::gateway::GatewayError::PrivateHistory) => {
                (StatusCode::FORBIDDEN, "private_history")
            }
            Self::Memory(crate::memory::MemoryError::Invalid) => {
                (StatusCode::BAD_REQUEST, "invalid_memory")
            }
            Self::Memory(crate::memory::MemoryError::Conflict) => {
                (StatusCode::CONFLICT, "memory_conflict")
            }
            Self::Memory(crate::memory::MemoryError::NotFound) => {
                (StatusCode::NOT_FOUND, "memory_not_found")
            }
            Self::Memory(crate::memory::MemoryError::Limit) => {
                (StatusCode::TOO_MANY_REQUESTS, "memory_limit")
            }
            Self::Memory(_) => (StatusCode::SERVICE_UNAVAILABLE, "memory_unavailable"),
            Self::Interaction(crate::gateway::GatewayError::Schedule(error)) => match error {
                crate::scheduler::ScheduleError::Invalid(_) => {
                    (StatusCode::BAD_REQUEST, "invalid_schedule")
                }
                crate::scheduler::ScheduleError::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
                crate::scheduler::ScheduleError::Conflict => {
                    (StatusCode::CONFLICT, "schedule_conflict")
                }
                crate::scheduler::ScheduleError::Limit => {
                    (StatusCode::TOO_MANY_REQUESTS, "schedule_limit")
                }
                _ => (StatusCode::SERVICE_UNAVAILABLE, "schedule_unavailable"),
            },
            Self::Aggregate(crate::aggregation::AggregateError::Identity(
                IdentityError::Unauthorized,
            )) => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::Aggregate(
                crate::aggregation::AggregateError::Identity(IdentityError::Forbidden)
                | crate::aggregation::AggregateError::Ownership,
            ) => (StatusCode::FORBIDDEN, "forbidden"),
            Self::Aggregate(crate::aggregation::AggregateError::Invalid) => {
                (StatusCode::BAD_REQUEST, "aggregate_invalid")
            }
            Self::Aggregate(crate::aggregation::AggregateError::Conflict) => {
                (StatusCode::CONFLICT, "aggregate_conflict")
            }
            Self::Aggregate(crate::aggregation::AggregateError::Unknown) => {
                (StatusCode::CONFLICT, "aggregate_unknown")
            }
            Self::Aggregate(crate::aggregation::AggregateError::Upstream(
                "authentication_rejected",
            )) => (StatusCode::BAD_GATEWAY, "aggregate_credentials"),
            Self::Aggregate(crate::aggregation::AggregateError::Upstream(
                "rate_limited" | "busy",
            )) => (StatusCode::TOO_MANY_REQUESTS, "aggregate_rate_limited"),
            Self::Aggregate(crate::aggregation::AggregateError::Upstream("operation_rejected")) => {
                (StatusCode::BAD_GATEWAY, "aggregate_rejected")
            }
            Self::Aggregate(_) => (StatusCode::BAD_GATEWAY, "aggregate_unavailable"),
            Self::Model(crate::models::ModelConfigError::Invalid) => {
                (StatusCode::BAD_REQUEST, "invalid_model")
            }
            Self::State(crate::state::StateError::Limit) => {
                (StatusCode::TOO_MANY_REQUESTS, "account_limit")
            }
            Self::State(crate::state::StateError::InvocationConflict) => {
                (StatusCode::CONFLICT, "data_conflict")
            }
            Self::Interaction(crate::gateway::GatewayError::State(
                crate::state::StateError::InvocationConflict,
            )) => (StatusCode::CONFLICT, "access_conflict"),
            Self::Interaction(crate::gateway::GatewayError::Invalid) => {
                (StatusCode::BAD_REQUEST, "invalid_workspace")
            }
            Self::Interaction(
                crate::gateway::GatewayError::RecoveryRequired
                | crate::gateway::GatewayError::Runtime(crate::RuntimeError::ExecutionUnconfirmed),
            ) => (StatusCode::CONFLICT, "execution_unknown"),
            Self::Interaction(
                crate::gateway::GatewayError::State(crate::state::StateError::Limit)
                | crate::gateway::GatewayError::Runtime(crate::RuntimeError::State(
                    crate::state::StateError::Limit,
                )),
            ) => (StatusCode::TOO_MANY_REQUESTS, "turn_limit"),
            Self::Interaction(
                crate::gateway::GatewayError::Closing
                | crate::gateway::GatewayError::Runtime(crate::RuntimeError::Closing),
            ) => (StatusCode::SERVICE_UNAVAILABLE, "service_closing"),
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
            Self::Identity(IdentityError::PasswordMismatch) => {
                (StatusCode::BAD_REQUEST, "password_mismatch")
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

/// The public authority identifies this deployment for every client. Native
/// clients instead arrive through a loopback SSH forward and carry the host's
/// own listener authority; browser routes must never accept that spelling.
fn host_matches(config: &super::PortalConfig, host: Option<&str>, native: bool) -> bool {
    host == Some(config.authority.as_str())
        || (native && config.listen_authority.as_deref() == host)
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
    let invalid_host = ambiguous || !host_matches(&portal.config, host, native);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_listener_is_accepted_only_for_native_endpoints() {
        let config = crate::portal::PortalConfig::new("https://agent.example.test")
            .unwrap()
            .with_listen_authority("127.0.0.1:47671".parse().unwrap());
        assert!(host_matches(&config, Some("agent.example.test"), false));
        assert!(host_matches(&config, Some("agent.example.test"), true));
        assert!(host_matches(&config, Some("127.0.0.1:47671"), true));
        assert!(!host_matches(&config, Some("127.0.0.1:47671"), false));
        assert!(!host_matches(&config, Some("127.0.0.1:47672"), true));
        assert!(!host_matches(&config, Some("attacker.example.test"), true));
        assert!(!host_matches(&config, None, true));
    }

    #[test]
    fn unconfigured_listener_authority_never_bypasses_the_public_host() {
        let config = crate::portal::PortalConfig::new("https://agent.example.test").unwrap();
        assert!(host_matches(&config, Some("agent.example.test"), true));
        assert!(!host_matches(&config, Some("127.0.0.1:47671"), true));
    }

    #[tokio::test]
    async fn task_outcome_errors_remain_distinct_from_identity_outages() {
        for (error, status, code) in [
            (
                crate::gateway::GatewayError::PrivateHistory,
                StatusCode::FORBIDDEN,
                "private_history",
            ),
            (
                crate::gateway::GatewayError::RecoveryRequired,
                StatusCode::CONFLICT,
                "execution_unknown",
            ),
            (
                crate::gateway::GatewayError::Runtime(crate::RuntimeError::ExecutionUnconfirmed),
                StatusCode::CONFLICT,
                "execution_unknown",
            ),
            (
                crate::gateway::GatewayError::Runtime(crate::RuntimeError::State(
                    crate::state::StateError::Limit,
                )),
                StatusCode::TOO_MANY_REQUESTS,
                "turn_limit",
            ),
            (
                crate::gateway::GatewayError::Closing,
                StatusCode::SERVICE_UNAVAILABLE,
                "service_closing",
            ),
        ] {
            let response = PortalError::Interaction(error).into_response();
            assert_eq!(response.status(), status);
            let bytes = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["error"], code);
        }
    }
}
