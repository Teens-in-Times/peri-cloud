//! Browser account/device entry point. No Agent loop, channel credentials or
//! tool policy is owned by HTTP requests; deployment retains those lifecycles.

mod browser;
mod interactions;
mod native;
mod security;

use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::middleware;
use axum::routing::{get, post};
use axum::Router;
use url::Url;

use crate::identity::IdentityService;

pub use security::PortalError;

/// The external browser origin, including an explicit non-default port.
/// HTTPS is required except for literal loopback HTTP carried through SSH.
#[derive(Clone)]
pub struct PortalConfig {
    pub(crate) origin: String,
    pub(crate) authority: String,
    pub(crate) secure: bool,
}

impl PortalConfig {
    pub fn new(public_origin: &str) -> Result<Self, PortalError> {
        let url = Url::parse(public_origin).map_err(|_| PortalError::Configuration)?;
        let loopback = matches!(url.host_str(), Some("127.0.0.1" | "[::1]"));
        if url.host_str().is_none()
            || !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(PortalError::Configuration);
        }
        let origin = url.origin().ascii_serialization();
        Ok(Self {
            authority: origin.split_once("://").unwrap().1.to_owned(),
            origin,
            secure: url.scheme() == "https",
        })
    }

    pub(crate) fn session_cookie(&self) -> &'static str {
        if self.secure {
            "__Host-peri_session"
        } else {
            "peri_session"
        }
    }

    pub(crate) fn csrf_cookie(&self) -> &'static str {
        if self.secure {
            "__Host-peri_csrf"
        } else {
            "peri_csrf"
        }
    }
}

#[derive(Clone)]
pub(crate) struct Portal {
    identity: Arc<IdentityService>,
    config: PortalConfig,
    gateway: Option<Arc<crate::gateway::Gateway>>,
    shutdown: Option<tokio_util::sync::CancellationToken>,
}

/// Serve embedded assets and identity APIs. Mount behind a proxy that preserves
/// Host, or use loopback plus SSH forwarding; forwarded headers aren't trusted.
pub fn router(identity: Arc<IdentityService>, config: PortalConfig) -> Router {
    build_router(Portal {
        identity,
        config,
        gateway: None,
        shutdown: None,
    })
}

/// Cloud deployments attach their existing gateway for account-scoped approval.
/// The independent OAuth/account entry point remains available without it.
pub fn router_with_gateway(
    identity: Arc<IdentityService>,
    config: PortalConfig,
    gateway: Arc<crate::gateway::Gateway>,
) -> Router {
    build_router(Portal {
        identity,
        config,
        gateway: Some(gateway),
        shutdown: None,
    })
}

/// Unified deployment account entry point, with an authenticated stop request.
pub fn router_for_host(
    identity: Arc<IdentityService>,
    config: PortalConfig,
    gateway: Arc<crate::gateway::Gateway>,
    shutdown: tokio_util::sync::CancellationToken,
) -> Router {
    build_router(Portal {
        identity,
        config,
        gateway: Some(gateway),
        shutdown: Some(shutdown),
    })
}

fn build_router(portal: Portal) -> Router {
    Router::new()
        .route("/", get(browser::page))
        .route("/portal.js", get(browser::javascript))
        .route("/portal.css", get(browser::stylesheet))
        .route("/api/status", get(browser::status))
        .route("/api/service/shutdown", post(interactions::shutdown))
        .route("/api/setup", post(browser::setup))
        .route("/api/login", post(browser::login))
        .route("/api/logout", post(browser::logout))
        .route("/api/account", get(browser::account))
        .route("/api/interactions", get(interactions::pending))
        .route(
            "/api/interactions/{id}/respond",
            post(interactions::respond),
        )
        .route("/api/devices/{id}/revoke", post(browser::revoke_device))
        .route("/api/pairing/code", post(browser::pair_code))
        .route("/api/pairing/{id}/confirm", post(browser::confirm_pair))
        .route("/api/channels/revoke", post(browser::revoke_channel))
        .route(
            "/oauth/authorize",
            get(native::consent_page).post(native::consent),
        )
        .route("/oauth/token", post(native::token))
        .route("/api/native/device", post(native::register_device))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::from_fn_with_state(
            portal.clone(),
            security::guard,
        ))
        .with_state(portal)
}
