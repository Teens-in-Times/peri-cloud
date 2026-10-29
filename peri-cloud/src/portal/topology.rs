use super::{security, Portal, PortalError};
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap},
    Json,
};
use serde_json::{json, Value};
use uuid::Uuid;

pub(super) async fn javascript() -> (HeaderMap, &'static str) {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        "text/javascript; charset=utf-8".parse().unwrap(),
    );
    (headers, include_str!("assets/workspace.js"))
}

pub(super) async fn check_connection(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    let gateway = portal.gateway.as_ref().ok_or(PortalError::Invalid)?;
    let transport = gateway.browser_check_connection(&actor, id).await?;
    Ok(Json(json!({"device_id":id,"transport":transport})))
}

pub(super) async fn inspect_device(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    let gateway = portal.gateway.as_ref().ok_or(PortalError::Invalid)?;
    Ok(Json(
        json!({"device_id":id,"check":gateway.browser_inspect_device(&actor,id).await?}),
    ))
}

pub(super) async fn bookmark(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(command): Json<crate::gateway::fleet::BookmarkCommand>,
) -> Result<Json<Value>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    let gateway = portal.gateway.as_ref().ok_or(PortalError::Invalid)?;
    Ok(Json(
        json!({"bookmarks":gateway.browser_bookmark(&actor,id,command).await.map_err(|error| match error {
            crate::gateway::GatewayError::Invalid => PortalError::Invalid,
            error => error.into(),
        })?}),
    ))
}
