//! Fixed management operations; no arbitrary New API proxy is exposed.
use super::{security, Portal, PortalError};
use crate::aggregation::{Aggregation, ChannelCommand, ChannelPage, ControlActor, ModelSelection};
use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

fn control(portal: &Portal) -> Result<Arc<Aggregation>, PortalError> {
    portal
        .models
        .as_ref()
        .and_then(|m| m.aggregation())
        .ok_or(PortalError::Invalid)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ModelScope {
    session: Option<Uuid>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Page {
    #[serde(default)]
    page: usize,
}

pub(super) async fn state(
    State(portal): State<Portal>,
    headers: HeaderMap,
) -> Result<Json<Value>, PortalError> {
    let actor = security::browser(&portal, &headers, false).await?;
    let Some(aggregate) = portal.models.as_ref().and_then(|m| m.aggregation()) else {
        return Ok(Json(json!({"enabled":false})));
    };
    let selection = aggregate
        .selection(&ControlActor::Browser(&actor), None)
        .await?;
    let models = aggregate.models(&ControlActor::Browser(&actor)).await;
    let (models, error) = match models {
        Ok(models) => (Some(models), None),
        Err(_) => (None, Some("catalog_unavailable")),
    };
    Ok(Json(
        json!({"enabled":true,"gateway":"New API","inference_base":aggregate.inference_base(),"selection":selection,"models":models,"catalog_error":error,"default_model":portal.models.as_ref().map(|m|m.default_model()),"admin":portal.identity.is_admin(&actor).await?}),
    ))
}
pub(super) async fn channels(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Query(page): Query<Page>,
) -> Result<Json<ChannelPage>, PortalError> {
    let actor = security::browser(&portal, &headers, false).await?;
    Ok(Json(
        control(&portal)?
            .channels(&ControlActor::Browser(&actor), page.page)
            .await?,
    ))
}
pub(super) async fn groups(
    State(portal): State<Portal>,
    headers: HeaderMap,
) -> Result<Json<Vec<String>>, PortalError> {
    let actor = security::browser(&portal, &headers, false).await?;
    Ok(Json(
        control(&portal)?
            .groups(&ControlActor::Browser(&actor))
            .await?,
    ))
}
pub(super) async fn channel_command(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Json(command): Json<ChannelCommand>,
) -> Result<Json<Value>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    Ok(Json(
        control(&portal)?
            .channel_command(&ControlActor::Browser(&actor), command)
            .await?,
    ))
}
pub(super) async fn selection(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Query(scope): Query<ModelScope>,
) -> Result<Json<ModelSelection>, PortalError> {
    let actor = security::browser(&portal, &headers, false).await?;
    Ok(Json(
        control(&portal)?
            .selection(&ControlActor::Browser(&actor), scope.session)
            .await?,
    ))
}
pub(super) async fn choose(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Query(scope): Query<ModelScope>,
    Json(input): Json<ModelSelection>,
) -> Result<Json<ModelSelection>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    Ok(Json(
        control(&portal)?
            .choose_model(&ControlActor::Browser(&actor), scope.session, input)
            .await?,
    ))
}

pub(super) async fn usage(
    State(portal): State<Portal>,
    headers: HeaderMap,
) -> Result<Json<Value>, PortalError> {
    let actor = security::browser(&portal, &headers, false).await?;
    Ok(Json(
        control(&portal)?
            .usage(&ControlActor::Browser(&actor))
            .await?,
    ))
}
pub(super) async fn audit(
    State(portal): State<Portal>,
    headers: HeaderMap,
) -> Result<Json<Value>, PortalError> {
    let actor = security::browser(&portal, &headers, false).await?;
    Ok(Json(
        control(&portal)?
            .audit(&ControlActor::Browser(&actor))
            .await?,
    ))
}

pub(super) async fn javascript() -> (HeaderMap, &'static str) {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        "text/javascript; charset=utf-8".parse().unwrap(),
    );
    (headers, include_str!("assets/models.js"))
}
