use super::{security, Portal, PortalError};
use crate::state::UserData;
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
pub(super) struct Register {
    invite_code: String,
    login: String,
    password: String,
    display_name: String,
}
pub(super) async fn register(
    State(portal): State<Portal>,
    Json(input): Json<Register>,
) -> Result<Json<Value>, PortalError> {
    let principal = portal
        .identity
        .register_invited(
            &input.invite_code,
            &input.login,
            input.password,
            &input.display_name,
        )
        .await?;
    Ok(Json(json!({"principal":principal})))
}
pub(super) async fn invite(
    State(portal): State<Portal>,
    headers: HeaderMap,
) -> Result<Json<Value>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    Ok(Json(
        serde_json::to_value(portal.identity.create_invite(&actor).await?)
            .map_err(|_| PortalError::Invalid)?,
    ))
}
pub(super) async fn revoke_invite(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    portal.identity.revoke_invite(&actor, id).await?;
    Ok(Json(json!({"ok":true})))
}
pub(super) async fn read(
    State(portal): State<Portal>,
    headers: HeaderMap,
) -> Result<Json<Value>, PortalError> {
    let actor = security::browser(&portal, &headers, false).await?;
    let gateway = portal.gateway.as_ref().ok_or(PortalError::Invalid)?;
    Ok(Json(
        json!({"data":gateway.journal().user_data(actor.principal().id).await?,
        "usage":gateway.journal().user_usage(actor.principal().id).await?,"limits":gateway.limits()}),
    ))
}
pub(super) async fn save(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Json(input): Json<UserData>,
) -> Result<Json<UserData>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    let gateway = portal.gateway.as_ref().ok_or(PortalError::Invalid)?;
    Ok(Json(
        gateway
            .journal()
            .save_user_data(
                actor.principal().id,
                input,
                gateway.limits().max_user_data_bytes,
            )
            .await?,
    ))
}

pub(super) async fn model(
    State(portal): State<Portal>,
    headers: HeaderMap,
) -> Result<Json<Value>, PortalError> {
    let actor = security::browser(&portal, &headers, false).await?;
    let Some(models) = &portal.models else {
        return Ok(Json(json!({"enabled":false})));
    };
    if models.aggregation().is_some() {
        return Ok(Json(json!({"enabled":true,"aggregated":true})));
    }
    Ok(Json(
        json!({"enabled":true,"providers":models.providers(),"default_model":models.default_model(),"choice":models.choice(actor.principal().id).await?,"custom_endpoint_allowed":portal.identity.is_admin(&actor).await?}),
    ))
}
pub(super) async fn save_model(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Json(input): Json<crate::models::ModelUpdate>,
) -> Result<Json<Value>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    if input.api_base.is_some() || input.protocol.is_some() {
        portal.identity.require_admin(&actor).await?;
    }
    portal
        .models
        .as_ref()
        .ok_or(PortalError::Invalid)?
        .save(actor.principal().id, input)
        .await?;
    Ok(Json(json!({"ok":true})))
}
pub(super) async fn reset_model(
    State(portal): State<Portal>,
    headers: HeaderMap,
) -> Result<Json<Value>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    portal
        .models
        .as_ref()
        .ok_or(PortalError::Invalid)?
        .reset(actor.principal().id)
        .await?;
    Ok(Json(json!({"ok":true})))
}
