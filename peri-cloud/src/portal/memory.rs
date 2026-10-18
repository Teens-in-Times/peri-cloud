use super::{security, Portal, PortalError};
use crate::memory::{MemoryContent, MemoryScope, MemorySource, MemoryStore};
use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

pub(super) async fn javascript() -> ([(axum::http::HeaderName, &'static str); 1], &'static str) {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/javascript; charset=utf-8",
        )],
        include_str!("assets/memory.js"),
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Search {
    #[serde(default = "user_scope")]
    scope: String,
    #[serde(default)]
    query: String,
    #[serde(default)]
    offset: usize,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Add {
    #[serde(default)]
    scope: MemoryScope,
    request_id: Uuid,
    generation: u64,
    value: MemoryContent,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Edit {
    #[serde(default)]
    scope: MemoryScope,
    revision: u64,
    value: MemoryContent,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Forget {
    #[serde(default)]
    scope: MemoryScope,
    revision: u64,
}
fn user_scope() -> String {
    "user".into()
}

async fn store(
    portal: &Portal,
    headers: &HeaderMap,
    write: bool,
) -> Result<MemoryStore, PortalError> {
    let actor = security::browser(portal, headers, write).await?;
    let gateway = portal.gateway.as_ref().ok_or(PortalError::Invalid)?;
    Ok(MemoryStore::new(
        gateway.journal(),
        actor.principal().id,
        gateway.limits().max_memory_bytes,
    )
    .for_management())
}
pub(super) async fn list(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Query(search): Query<Search>,
) -> Result<Json<Value>, PortalError> {
    let store = store(&portal, &headers, false)
        .await?
        .scoped(MemoryScope::from_key(&search.scope)?);
    let page = store.inventory(&search.query, search.offset, 10).await?;
    let gateway = portal.gateway.as_ref().ok_or(PortalError::Invalid)?;
    let actor = security::browser(&portal, &headers, false).await?;
    let mut scopes = vec![
        json!({"key":"user","label":"我的资料","scope":{"kind":"user"}}),
        json!({"key":"fleet","label":"我的设备池","scope":{"kind":"fleet"}}),
    ];
    for device in crate::memory::scope::owned_devices(
        &gateway.journal().identity_pool(),
        actor.principal().id,
    )
    .await?
    {
        scopes.push(json!({"key":format!("device_env:{}",device.id),"label":if device.revoked { format!("{}（已撤销，可查看或忘记）",device.name) } else {device.name},"read_only":device.revoked,"scope":{"kind":"device_env","device_id":device.id}}));
    }
    Ok(Json(
        json!({"page":page,"scopes":scopes,"max_bytes":gateway.limits().max_memory_bytes,"max_items":crate::memory::MAX_ITEMS}),
    ))
}
pub(super) async fn get(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(search): Query<Search>,
) -> Result<Json<Value>, PortalError> {
    Ok(Json(json!(
        store(&portal, &headers, false)
            .await?
            .scoped(MemoryScope::from_key(&search.scope)?)
            .inspect(id)
            .await?
    )))
}
pub(super) async fn add(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Json(add): Json<Add>,
) -> Result<Json<Value>, PortalError> {
    Ok(Json(json!(
        store(&portal, &headers, true)
            .await?
            .scoped(add.scope)
            .add(
                add.request_id,
                add.generation,
                add.value,
                MemorySource::web()
            )
            .await?
    )))
}
pub(super) async fn edit(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(edit): Json<Edit>,
) -> Result<Json<Value>, PortalError> {
    Ok(Json(json!(
        store(&portal, &headers, true)
            .await?
            .scoped(edit.scope)
            .update(id, edit.revision, edit.value, MemorySource::web())
            .await?
    )))
}
pub(super) async fn forget(
    State(portal): State<Portal>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(forget): Json<Forget>,
) -> Result<Json<Value>, PortalError> {
    store(&portal, &headers, true)
        .await?
        .scoped(forget.scope)
        .forget(id, forget.revision, MemorySource::web())
        .await?;
    Ok(Json(json!({"forgotten":true})))
}
