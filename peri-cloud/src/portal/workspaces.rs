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
    workspace: String,
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
        .switch_browser_workspace(&actor, expected, input.workspace)
        .await?;
    Ok(Json(json!({"session_id": session})))
}
