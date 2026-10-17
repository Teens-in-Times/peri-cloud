use super::{security, Portal, PortalError};
use crate::gateway::TaskReceipt;
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NewTask {
    request_id: Uuid,
    confirmed: bool,
    prompt: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OpenWorkspace {
    request_id: Uuid,
    device_id: Uuid,
    workspace: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CancelTask {
    turn_id: Uuid,
    confirmed: bool,
}
pub(super) async fn submit(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Path(session): Path<Uuid>,
    Json(input): Json<NewTask>,
) -> Result<Json<TaskReceipt>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    if !input.confirmed {
        return Err(PortalError::Invalid);
    }
    let gateway = portal.gateway.as_ref().ok_or(PortalError::Invalid)?;
    Ok(Json(
        gateway
            .submit_browser_task(&actor, session, input.request_id, input.prompt)
            .await?,
    ))
}
pub(super) async fn cancel(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Path(session): Path<Uuid>,
    Json(input): Json<CancelTask>,
) -> Result<Json<TaskReceipt>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    if !input.confirmed {
        return Err(PortalError::Invalid);
    }
    let gateway = portal.gateway.as_ref().ok_or(PortalError::Invalid)?;
    Ok(Json(
        gateway
            .cancel_browser_task(&actor, session, input.turn_id)
            .await?,
    ))
}
pub(super) async fn open(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Json(input): Json<OpenWorkspace>,
) -> Result<Json<Value>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    let gateway = portal.gateway.as_ref().ok_or(PortalError::Invalid)?;
    let session = gateway
        .open_browser_workspace(&actor, input.request_id, input.device_id, input.workspace)
        .await?;
    Ok(Json(json!({"session_id":session})))
}
