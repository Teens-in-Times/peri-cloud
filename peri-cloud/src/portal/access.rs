use super::{security, Portal, PortalError};
use crate::gateway::access::{AccessChange, AccessRecord, GatewayAccess};
use axum::{extract::State, http::HeaderMap, Json};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Change {
    adapter: String,
    revision: u64,
    change: AccessChange,
}

pub(super) async fn list(
    State(portal): State<Portal>,
    headers: HeaderMap,
) -> Result<Json<Vec<GatewayAccess>>, PortalError> {
    let actor = security::browser(&portal, &headers, false).await?;
    Ok(Json(
        portal
            .gateway
            .as_ref()
            .ok_or(PortalError::Invalid)?
            .browser_access(&actor)
            .await?,
    ))
}
pub(super) async fn change(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Json(input): Json<Change>,
) -> Result<Json<AccessRecord>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    Ok(Json(
        portal
            .gateway
            .as_ref()
            .ok_or(PortalError::Invalid)?
            .change_browser_access(&actor, &input.adapter, input.revision, &input.change)
            .await?,
    ))
}
