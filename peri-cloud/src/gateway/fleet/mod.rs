//! Account-scoped device observations and convenience metadata; never execution authority.
mod bookmarks;
pub(crate) use bookmarks::migrate;
pub use bookmarks::{Bookmark, BookmarkCommand};

use super::{Gateway, GatewayError, GatewayResult};
use crate::identity::{Authenticated, IdentityError};
use serde::Serialize;
use sqlx::Row;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Clone, Default, Serialize)]
pub struct DeviceInspection {
    pub transport: String,
    pub protocol_version: Option<u32>,
    pub version: Option<String>,
    pub features: Vec<String>,
    /// Advertised tools, not permission grants or a promise about a workspace.
    pub tools: Option<Vec<String>>,
}

#[derive(Default, Serialize)]
pub struct ConnectionLease {
    pub source: String,
    pub reported_at: Option<i64>,
    pub expires_at: Option<i64>,
}

pub(super) struct Observation {
    principal: Uuid,
    at: i64,
    since: Instant,
    details: Option<DeviceInspection>,
}

#[derive(Serialize)]
pub struct CheckView {
    pub observed_at: i64,
    pub fresh: bool,
    pub authenticated: bool,
    pub details: Option<DeviceInspection>,
}

#[derive(Default, Serialize)]
pub struct DeviceWork {
    pub mutation_lease: Option<String>,
    pub mutation_owner_session: Option<String>,
    pub unconfirmed_operations: i64,
    pub task_states: HashMap<String, i64>,
}

#[derive(Serialize)]
pub struct DeviceManagement {
    pub observed_at: i64,
    pub lease: ConnectionLease,
    pub check: Option<CheckView>,
    pub work: DeviceWork,
    pub bookmarks: Vec<Bookmark>,
    pub known_workspaces: Vec<String>,
    pub recommendation: Option<&'static str>,
}

impl Gateway {
    /// Does not create a session, job, model request, or grant tool permission.
    pub async fn browser_inspect_device(
        &self,
        actor: &Authenticated,
        id: Uuid,
    ) -> GatewayResult<CheckView> {
        self.identity.browser_scope(actor).await?;
        let devices = self.identity.devices(actor).await?;
        let device = devices
            .iter()
            .find(|d| d.id == id && !d.revoked)
            .ok_or(IdentityError::Forbidden)?;
        let result = tokio::time::timeout(
            Duration::from_secs(6),
            self.connector.inspect_device(device),
        )
        .await;
        if !self
            .identity
            .devices(actor)
            .await?
            .iter()
            .any(|d| d.id == id && !d.revoked)
        {
            return Err(IdentityError::Forbidden.into());
        }
        let details = match result {
            Ok(Ok(value)) => Some(value),
            _ => None,
        };
        let view = CheckView {
            observed_at: self.identity.now(),
            fresh: true,
            authenticated: details.is_some(),
            details: details.clone(),
        };
        let mut observed = self.device_observations.write();
        observed.retain(|_, o| o.since.elapsed() < Duration::from_secs(300));
        if observed.len() >= 512 && !observed.contains_key(&id) {
            if let Some(oldest) = observed
                .iter()
                .min_by_key(|(_, o)| o.since)
                .map(|(id, _)| *id)
            {
                observed.remove(&oldest);
            }
        }
        observed.insert(
            id,
            Observation {
                principal: actor.principal().id,
                at: view.observed_at,
                since: Instant::now(),
                details,
            },
        );
        Ok(view)
    }

    pub async fn browser_device_management(
        &self,
        actor: &Authenticated,
    ) -> GatewayResult<HashMap<Uuid, DeviceManagement>> {
        self.identity.browser_scope(actor).await?;
        let principal = actor.principal().id;
        let now = self.identity.now();
        let mut result = HashMap::new();
        for device in self.identity.devices(actor).await? {
            if device.revoked {
                continue;
            }
            let check = self
                .device_observations
                .read()
                .get(&device.id)
                .filter(|o| o.principal == principal)
                .map(|o| CheckView {
                    observed_at: o.at,
                    fresh: o.since.elapsed() < Duration::from_secs(60)
                        && (0..60).contains(&now.saturating_sub(o.at)),
                    authenticated: o.details.is_some(),
                    details: o.details.clone(),
                });
            let work = self.device_work(principal, device.id).await?;
            let recommendation = check
                .as_ref()
                .filter(|c| c.fresh && c.authenticated)
                .and_then(|c| c.details.as_ref())
                .and_then(|d| d.tools.as_ref())
                .filter(|tools| !tools.is_empty())
                .filter(|_| work.mutation_lease.is_none() && work.unconfirmed_operations == 0)
                .map(|_| "recently_verified_idle_device_for_new_task");
            let known_workspaces = sqlx::query_scalar("SELECT json_extract(state_json,'$.frozen.binding.workspace') AS workspace FROM cloud_sessions WHERE principal=? AND json_extract(state_json,'$.frozen.device_id')=? AND workspace IS NOT NULL AND workspace!='' GROUP BY workspace ORDER BY MAX(rowid) DESC LIMIT 16")
                .bind(principal.to_string()).bind(device.id.to_string()).fetch_all(&self.store.pool).await?;
            result.insert(
                device.id,
                DeviceManagement {
                    observed_at: now,
                    lease: self.connector.connection_lease(principal, &device).await?,
                    check,
                    work,
                    bookmarks: self.device_bookmarks(principal, device.id).await?,
                    known_workspaces,
                    recommendation,
                },
            );
        }
        // A revocation during an awaited read wins over the earlier snapshot.
        let visible = self.identity.devices(actor).await?;
        result.retain(|id, _| visible.iter().any(|d| d.id == *id && !d.revoked));
        Ok(result)
    }

    async fn device_work(&self, principal: Uuid, device: Uuid) -> GatewayResult<DeviceWork> {
        let lease: Option<(String, String)> = sqlx::query_as(
            "SELECT phase,owner_session FROM agent_device_leases WHERE principal=? AND device_id=?",
        )
        .bind(principal.to_string())
        .bind(device.to_string())
        .fetch_optional(&self.store.pool)
        .await?;
        let mut work = DeviceWork {
            mutation_lease: lease.as_ref().map(|l| l.0.clone()),
            mutation_owner_session: lease.map(|l| l.1),
            unconfirmed_operations: sqlx::query_scalar("SELECT COUNT(*) FROM cloud_native_operations WHERE principal=? AND device_id=? AND state!='confirmed'")
                .bind(principal.to_string()).bind(device.to_string()).fetch_one(&self.store.pool).await?,
            ..Default::default()
        };
        let rows = sqlx::query("SELECT json_extract(t.record_json,'$.state') AS state,COUNT(*) AS total FROM agent_tasks t JOIN cloud_sessions s ON s.id=t.worker_session AND s.principal=t.principal WHERE t.principal=? AND json_extract(s.state_json,'$.frozen.device_id')=? AND json_extract(t.record_json,'$.state') NOT IN ('completed','failed','cancelled','invalidated') GROUP BY state LIMIT 32")
            .bind(principal.to_string()).bind(device.to_string()).fetch_all(&self.store.pool).await?;
        for row in rows {
            work.task_states.insert(row.get("state"), row.get("total"));
        }
        Ok(work)
    }
}
