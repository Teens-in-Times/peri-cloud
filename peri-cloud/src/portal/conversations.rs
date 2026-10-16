use super::{security, Portal, PortalError};
use crate::gateway::{ConversationActivity, SavedResultPage};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ResultQuery {
    #[serde(default = "first_page")]
    page: usize,
    turn: Option<Uuid>,
}
fn first_page() -> usize {
    1
}

pub(super) async fn list(
    State(portal): State<Portal>,
    headers: HeaderMap,
) -> Result<Json<Vec<ConversationActivity>>, PortalError> {
    let actor = security::browser(&portal, &headers, false).await?;
    Ok(Json(match &portal.gateway {
        Some(gateway) => gateway.browser_activity(&actor).await?,
        None => Vec::new(),
    }))
}

pub(super) async fn result(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Path(session): Path<Uuid>,
    Query(query): Query<ResultQuery>,
) -> Result<Json<Option<SavedResultPage>>, PortalError> {
    let actor = security::browser(&portal, &headers, false).await?;
    let gateway = portal.gateway.as_ref().ok_or(PortalError::Invalid)?;
    Ok(Json(
        gateway
            .browser_result(&actor, session, query.page, query.turn)
            .await?,
    ))
}
