use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::Json;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{security, Portal, PortalError};
use crate::gateway::{InteractionAction, InteractionCard};

pub(super) async fn pending(
    State(portal): State<Portal>,
    headers: HeaderMap,
) -> Result<Json<Vec<InteractionCard>>, PortalError> {
    let actor = security::browser(&portal, &headers, false).await?;
    let cards = match &portal.gateway {
        Some(gateway) => gateway.browser_interactions(&actor).await?,
        None => Vec::new(),
    };
    Ok(Json(cards))
}

pub(super) async fn respond(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Path(request): Path<Uuid>,
    Json(action): Json<InteractionAction>,
) -> Result<Json<Value>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    let gateway = portal.gateway.as_ref().ok_or(PortalError::Invalid)?;
    gateway.respond_browser(&actor, request, action).await?;
    Ok(Json(json!({"ok":true})))
}

pub(super) async fn shutdown(
    State(portal): State<Portal>,
    headers: HeaderMap,
) -> Result<Json<Value>, PortalError> {
    security::browser(&portal, &headers, true).await?;
    let shutdown = portal.shutdown.as_ref().ok_or(PortalError::Invalid)?;
    shutdown.cancel();
    Ok(Json(json!({"status":"shutdown_requested"})))
}
