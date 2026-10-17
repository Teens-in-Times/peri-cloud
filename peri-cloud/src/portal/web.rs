//! Authenticated Web channel API. Every route and sender is derived from the
//! browser session; page input only supplies text and a device choice.

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{security, Portal, PortalError};
use crate::gateway::{WebChat, WebSendResult};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct History {
    after: Option<i64>,
}

pub(super) async fn chat(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Query(query): Query<History>,
) -> Result<Json<WebChat>, PortalError> {
    let actor = security::browser(&portal, &headers, false).await?;
    let gateway = portal.gateway.as_ref().ok_or(PortalError::Invalid)?;
    Ok(Json(
        gateway
            .web_chat(&actor, query.after.filter(|value| *value >= 0))
            .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Selection {
    device_id: Option<Uuid>,
    workspace: Option<String>,
}

pub(super) async fn select(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Json(input): Json<Selection>,
) -> Result<Json<Value>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    let gateway = portal.gateway.as_ref().ok_or(PortalError::Invalid)?;
    let session = gateway
        .web_select(&actor, input.device_id, input.workspace)
        .await?;
    Ok(Json(json!({"session_id": session})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Message {
    text: String,
}

pub(super) async fn send(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Json(input): Json<Message>,
) -> Result<Json<WebSendResult>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    let gateway = portal.gateway.as_ref().ok_or(PortalError::Invalid)?;
    Ok(Json(gateway.web_send(&actor, input.text).await?))
}
