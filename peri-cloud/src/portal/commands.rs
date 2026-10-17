use super::{security, Portal, PortalError};
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use serde::Deserialize;
use uuid::Uuid;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ExecuteCommand {
    request_id: Uuid,
    command: String,
    confirmed: bool,
}
pub(super) async fn list(
    State(portal): State<Portal>,
    headers: HeaderMap,
) -> Result<Json<Vec<crate::commands::CommandDescriptor>>, PortalError> {
    security::browser(&portal, &headers, false).await?;
    Ok(Json(
        portal
            .gateway
            .as_ref()
            .ok_or(PortalError::Invalid)?
            .command_catalog(),
    ))
}
pub(super) async fn execute(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Path(session): Path<Uuid>,
    Json(input): Json<ExecuteCommand>,
) -> Result<Json<crate::gateway::CommandReceipt>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    if !input.confirmed {
        return Err(PortalError::Invalid);
    }
    Ok(Json(
        portal
            .gateway
            .as_ref()
            .ok_or(PortalError::Invalid)?
            .execute_browser_command(&actor, session, input.request_id, input.command)
            .await?,
    ))
}
