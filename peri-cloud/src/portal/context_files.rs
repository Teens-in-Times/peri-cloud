use super::{security, Portal, PortalError};
use crate::context::ContextDocument;
use axum::{extract::State, http::HeaderMap, Json};

pub(super) async fn list(
    State(portal): State<Portal>,
    headers: HeaderMap,
) -> Result<Json<Vec<ContextDocument>>, PortalError> {
    let actor = security::browser(&portal, &headers, false).await?;
    Ok(Json(
        portal
            .gateway
            .as_ref()
            .ok_or(PortalError::Invalid)?
            .context_documents(&actor)
            .await?,
    ))
}
pub(super) async fn save(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Json(document): Json<ContextDocument>,
) -> Result<Json<serde_json::Value>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    portal
        .gateway
        .as_ref()
        .ok_or(PortalError::Invalid)?
        .save_context_document(&actor, document)
        .await?;
    Ok(Json(
        serde_json::json!({"saved":true,"applies":"next_turn"}),
    ))
}
