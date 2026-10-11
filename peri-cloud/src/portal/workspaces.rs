use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{security, Portal, PortalError};
use crate::gateway::WorkspaceSelection;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SwitchWorkspace {
    workspace: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Browse {
    path: Option<String>,
    #[serde(default)]
    offset: usize,
}

pub(super) async fn browse(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<Browse>,
) -> Result<Json<peri_acp_types::device_executor::DirectoryListing>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    let gateway = portal.gateway.as_ref().ok_or(PortalError::Invalid)?;
    Ok(Json(
        gateway
            .browse_browser_instance(&actor, id, input.path, input.offset)
            .await?,
    ))
}

pub(super) async fn set_default(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<SwitchWorkspace>,
) -> Result<Json<Value>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    let gateway = portal.gateway.as_ref().ok_or(PortalError::Invalid)?;
    let path = gateway
        .set_browser_instance_default(&actor, id, input.workspace.ok_or(PortalError::Invalid)?)
        .await?;
    Ok(Json(json!({"workspace":path})))
}

pub(super) async fn list(
    State(portal): State<Portal>,
    headers: HeaderMap,
) -> Result<Json<Vec<WorkspaceSelection>>, PortalError> {
    let actor = security::browser(&portal, &headers, false).await?;
    let selections = match &portal.gateway {
        Some(gateway) => gateway.browser_workspaces(&actor).await?,
        None => Vec::new(),
    };
    Ok(Json(selections))
}

pub(super) async fn switch(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Path(expected): Path<Uuid>,
    Json(input): Json<SwitchWorkspace>,
) -> Result<Json<Value>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    let gateway = portal.gateway.as_ref().ok_or(PortalError::Invalid)?;
    let session = gateway
        .switch_browser_workspace(&actor, expected, input.workspace.unwrap_or_default())
        .await?;
    Ok(Json(json!({"session_id": session})))
}
