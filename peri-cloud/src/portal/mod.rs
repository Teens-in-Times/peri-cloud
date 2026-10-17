//! Browser account/device entry point. No Agent loop, channel credentials or
//! tool policy is owned by HTTP requests; deployment retains those lifecycles.

mod access;
mod aggregate;
mod browser;
mod conversations;
mod interactions;
mod native;
mod personal;
mod security;
mod tasks;
mod topology;
mod workspaces;

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
    pub(crate) service_name: String,
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
            service_name: "云端主控".into(),
            authority: origin.split_once("://").unwrap().1.to_owned(),
            origin,
            secure: url.scheme() == "https",
        })
    }

    pub fn with_service_name(mut self, name: Option<&str>) -> Self {
        if let Some(name) = name {
            self.service_name = name.to_owned();
        }
        self
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
    models: Option<Arc<crate::models::AccountModels>>,
}

/// Serve embedded assets and identity APIs. Mount behind a proxy that preserves
/// Host, or use loopback plus SSH forwarding; forwarded headers aren't trusted.
pub fn router(identity: Arc<IdentityService>, config: PortalConfig) -> Router {
    build_router(Portal {
        identity,
        config,
        gateway: None,
        shutdown: None,
        models: None,
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
        models: None,
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
        models: None,
    })
}

pub fn router_for_host_with_models(
    identity: Arc<IdentityService>,
    config: PortalConfig,
    gateway: Arc<crate::gateway::Gateway>,
    shutdown: tokio_util::sync::CancellationToken,
    models: Arc<crate::models::AccountModels>,
) -> Router {
    build_router(Portal {
        identity,
        config,
        gateway: Some(gateway),
        shutdown: Some(shutdown),
        models: Some(models),
    })
}

fn build_router(portal: Portal) -> Router {
    Router::new()
        .route("/", get(browser::page))
        .route("/portal.js", get(browser::javascript))
        .route("/workspace.js", get(topology::javascript))
        .route("/models.js", get(aggregate::javascript))
        .route("/portal.css", get(browser::stylesheet))
        .route("/api/status", get(browser::status))
        .route("/api/aggregate", get(aggregate::state))
        .route("/api/aggregate/channels", get(aggregate::channels))
        .route("/api/aggregate/groups", get(aggregate::groups))
        .route("/api/aggregate/usage", get(aggregate::usage))
        .route("/api/aggregate/audit", get(aggregate::audit))
        .route(
            "/api/aggregate/channel-command",
            post(aggregate::channel_command).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/api/aggregate/model",
            get(aggregate::selection).post(aggregate::choose),
        )
        .route("/api/service/shutdown", post(interactions::shutdown))
        .route("/api/setup", post(browser::setup))
        .route("/api/login", post(browser::login))
        .route("/api/register", post(personal::register))
        .route("/api/admin/invites", post(personal::invite))
        .route(
            "/api/admin/gateway-access",
            get(access::list).post(access::change),
        )
        .route(
            "/api/admin/invites/{id}/revoke",
            post(personal::revoke_invite),
        )
        .route(
            "/api/user-data",
            get(personal::read)
                .post(personal::save)
                .layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/api/model",
            get(personal::model).post(personal::save_model),
        )
        .route("/api/model/reset", post(personal::reset_model))
        .route("/api/logout", post(browser::logout))
        .route("/api/account", get(browser::account))
        .route(
            "/api/instances/{id}/connection-check",
            post(topology::check_connection),
        )
        .route("/api/workspaces", get(workspaces::list))
        .route("/api/workspaces/open", post(tasks::open))
        .route(
            "/api/conversations/{id}/tasks",
            post(tasks::submit).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route("/api/conversations/{id}/cancel", post(tasks::cancel))
        .route("/api/conversations", get(conversations::list))
        .route("/api/conversations/{id}/result", get(conversations::result))
        .route("/api/instances/{id}/browse", post(workspaces::browse))
        .route(
            "/api/instances/{id}/default-workspace",
            post(workspaces::set_default),
        )
        .route("/api/workspaces/{id}/switch", post(workspaces::switch))
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
