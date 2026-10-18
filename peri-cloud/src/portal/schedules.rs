use super::{security, Portal, PortalError};
use crate::scheduler::{NewSchedule, Schedule, ScheduleRun};
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use serde::Deserialize;
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ContactChange {
    confirmed: bool,
    policy: crate::scheduler::ContactPolicy,
}
pub(super) async fn contact_policy(
    State(portal): State<Portal>,
    headers: HeaderMap,
) -> Result<Json<crate::scheduler::ContactPolicy>, PortalError> {
    let actor = security::browser(&portal, &headers, false).await?;
    Ok(Json(
        portal
            .gateway
            .as_ref()
            .ok_or(PortalError::Invalid)?
            .browser_contact_policy(&actor)
            .await?,
    ))
}
pub(super) async fn save_contact_policy(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Json(input): Json<ContactChange>,
) -> Result<Json<crate::scheduler::ContactPolicy>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    if !input.confirmed {
        return Err(PortalError::Invalid);
    }
    Ok(Json(
        portal
            .gateway
            .as_ref()
            .ok_or(PortalError::Invalid)?
            .update_browser_contact_policy(&actor, input.policy)
            .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Create {
    confirmed: bool,
    schedule: NewSchedule,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Change {
    confirmed: bool,
    revision: i64,
    action: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Edit {
    confirmed: bool,
    edit: crate::scheduler::ScheduleEdit,
}
pub(super) async fn edit(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<Edit>,
) -> Result<Json<Schedule>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    if !input.confirmed {
        return Err(PortalError::Invalid);
    }
    Ok(Json(
        portal
            .gateway
            .as_ref()
            .ok_or(PortalError::Invalid)?
            .edit_browser_schedule(&actor, id, input.edit)
            .await?,
    ))
}
pub(super) async fn list(
    State(portal): State<Portal>,
    headers: HeaderMap,
) -> Result<Json<Vec<Schedule>>, PortalError> {
    let actor = security::browser(&portal, &headers, false).await?;
    Ok(Json(
        portal
            .gateway
            .as_ref()
            .ok_or(PortalError::Invalid)?
            .browser_schedules(&actor)
            .await?,
    ))
}
pub(super) async fn create(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Json(input): Json<Create>,
) -> Result<Json<Schedule>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    if !input.confirmed {
        return Err(PortalError::Invalid);
    }
    Ok(Json(
        portal
            .gateway
            .as_ref()
            .ok_or(PortalError::Invalid)?
            .create_browser_schedule(&actor, input.schedule)
            .await?,
    ))
}
pub(super) async fn change(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<Change>,
) -> Result<Json<Schedule>, PortalError> {
    let actor = security::browser(&portal, &headers, true).await?;
    if !input.confirmed {
        return Err(PortalError::Invalid);
    }
    Ok(Json(
        portal
            .gateway
            .as_ref()
            .ok_or(PortalError::Invalid)?
            .change_browser_schedule(&actor, id, input.revision, &input.action)
            .await?,
    ))
}
pub(super) async fn runs(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<ScheduleRun>>, PortalError> {
    let actor = security::browser(&portal, &headers, false).await?;
    Ok(Json(
        portal
            .gateway
            .as_ref()
            .ok_or(PortalError::Invalid)?
            .browser_schedule_runs(&actor, id)
            .await?,
    ))
}
