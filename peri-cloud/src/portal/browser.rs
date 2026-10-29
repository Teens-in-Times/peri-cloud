use axum::extract::{Path, State};
use axum::http::{header, HeaderMap};
use axum::response::Html;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{security, Portal, PortalError};
use crate::identity::{ChannelIdentity, DeviceRecord};

pub(super) async fn page() -> Html<&'static str> {
    Html(include_str!("assets/index.html"))
}

pub(super) async fn javascript() -> (HeaderMap, &'static str) {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        "text/javascript; charset=utf-8".parse().unwrap(),
    );
    (headers, include_str!("assets/portal.js"))
}

pub(super) async fn stylesheet() -> (HeaderMap, &'static str) {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        "text/css; charset=utf-8".parse().unwrap(),
    );
    (headers, include_str!("assets/portal.css"))
}

pub(super) async fn status(State(portal): State<Portal>) -> Result<Json<Value>, PortalError> {
    Ok(Json(json!({
        "initialized": portal.identity.initialized().await?,
        "csrf_cookie": portal.config.csrf_cookie(),
        "service_control": portal.shutdown.is_some(),
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Setup {
    bootstrap_token: String,
    login: String,
    password: String,
    display_name: String,
}

pub(super) async fn setup(
    State(portal): State<Portal>,
    Json(input): Json<Setup>,
) -> Result<Json<Value>, PortalError> {
    let principal = portal
        .identity
        .initialize(
            &input.bootstrap_token,
            &input.login,
            input.password,
            &input.display_name,
        )
        .await?;
    Ok(Json(json!({"principal": principal})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Login {
    login: String,
    password: String,
}

pub(super) async fn login(
    State(portal): State<Portal>,
    Json(input): Json<Login>,
) -> Result<(HeaderMap, Json<Value>), PortalError> {
    let receipt = portal.identity.login(&input.login, input.password).await?;
    let cookies = security::set_cookies(
        &portal,
        &receipt.session_token,
        &receipt.csrf_token,
        receipt.expires_in,
    );
    Ok((cookies, Json(json!({"principal": receipt.principal}))))
}

pub(super) async fn logout(
    State(portal): State<Portal>,
    headers: HeaderMap,
) -> Result<(HeaderMap, Json<Value>), PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    portal.identity.logout(&actor).await?;
    Ok((
        security::set_cookies(&portal, "", "", 0),
        Json(json!({"ok": true})),
    ))
}

pub(super) async fn account(
    State(portal): State<Portal>,
    headers: HeaderMap,
) -> Result<Json<Value>, PortalError> {
    let actor = security::browser(&portal, &headers, false).await?;
    let devices = portal.identity.devices(&actor).await?;
    let device_connections = match &portal.gateway {
        Some(gateway) => gateway.browser_device_states(&actor).await?,
        None => std::collections::HashMap::new(),
    };
    let gateways = match &portal.gateway {
        Some(gateway) => gateway.browser_gateways(&actor).await?,
        None => Vec::new(),
    };
    let device_management = match &portal.gateway {
        Some(gateway) => gateway.browser_device_management(&actor).await?,
        None => std::collections::HashMap::new(),
    };
    Ok(Json(json!({
        "service": {"name":portal.config.service_name,"origin":portal.config.origin,"version":env!("CARGO_PKG_VERSION"),"agent_enabled":portal.gateway.is_some(),"model_gateway":portal.models.as_ref().and_then(|m|m.aggregation()).is_some()},
        "gateways": gateways,
        "principal": actor.principal(),
        "is_admin": portal.identity.is_admin(&actor).await?,
        "personal_data": portal.gateway.is_some(),
        "devices": devices,
        "device_connections": device_connections,
        "device_management": device_management,
        "channels": portal.identity.channels(&actor).await?.into_iter().filter(|c|c.adapter_instance_id!=crate::gateway::WEB_ADAPTER).collect::<Vec<_>>(),
        "pending_pair_claims": portal.identity.pending_pair_claims(&actor).await?,
    })))
}

pub(super) async fn revoke_device(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    portal.identity.revoke_device(&actor, id).await?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ChangePassword {
    current_password: String,
    new_password: String,
}

pub(super) async fn change_password(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Json(input): Json<ChangePassword>,
) -> Result<Json<Value>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    portal
        .identity
        .change_password(&actor, input.current_password, input.new_password)
        .await?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Recover {
    bootstrap_token: String,
    login: String,
    new_password: String,
}

/// Operator password recovery. The deployment bootstrap token is the private
/// root of trust; no public email or reset link is exposed.
pub(super) async fn recover(
    State(portal): State<Portal>,
    Json(input): Json<Recover>,
) -> Result<Json<Value>, PortalError> {
    let principal = portal
        .identity
        .reset_password(&input.bootstrap_token, &input.login, input.new_password)
        .await?;
    Ok(Json(json!({"principal": principal})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RenameDevice {
    name: String,
}

pub(super) async fn rename_device(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<RenameDevice>,
) -> Result<Json<DeviceRecord>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    Ok(Json(
        portal
            .identity
            .rename_device(&actor, id, &input.name)
            .await?,
    ))
}

pub(super) async fn delete_device(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    portal.identity.delete_device(&actor, id).await?;
    Ok(Json(json!({"ok": true})))
}

pub(super) async fn pair_code(
    State(portal): State<Portal>,
    headers: HeaderMap,
) -> Result<Json<crate::identity::PairCode>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    Ok(Json(portal.identity.create_pair_code(&actor).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Confirmation {
    allow: bool,
}

pub(super) async fn confirm_pair(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<Confirmation>,
) -> Result<Json<Value>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    portal
        .identity
        .confirm_pair_claim(&actor, id, input.allow)
        .await?;
    Ok(Json(json!({"ok": true})))
}

pub(super) async fn revoke_channel(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Json(identity): Json<ChannelIdentity>,
) -> Result<Json<Value>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    portal.identity.revoke_channel(&actor, &identity).await?;
    Ok(Json(json!({"ok": true})))
}
