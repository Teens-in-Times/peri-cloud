use std::sync::Arc;

use peri_acp_types::permission::PermissionMode;
use serde::Serialize;
use tokio::sync::oneshot;
use uuid::Uuid;

use super::Gateway;
use crate::gateway::{ChannelRoute, GatewayError, GatewayResult};
use crate::identity::{Authenticated, DeviceRecord, IdentityError};
use crate::state::FrozenSession;

#[derive(Serialize)]
pub struct WorkspaceSelection {
    pub session_id: Uuid,
    pub device_id: Uuid,
    pub device_name: String,
    pub platform: String,
    pub adapter: String,
    pub conversation_id: String,
    pub workspace: String,
    pub default_workspace: String,
    pub instance_kind: String,
    pub known_workspaces: Vec<String>,
    pub busy: bool,
}

impl Gateway {
    pub async fn browse_browser_instance(
        &self,
        actor: &Authenticated,
        id: Uuid,
        path: Option<String>,
        offset: usize,
    ) -> GatewayResult<peri_acp_types::device_executor::DirectoryListing> {
        self.identity.browser_scope(actor).await?;
        let devices = self.identity.devices(actor).await?;
        let device = devices
            .iter()
            .find(|d| d.id == id && !d.revoked)
            .ok_or(IdentityError::Forbidden)?;
        let path = path.unwrap_or_else(|| device.default_workspace.clone());
        if !path.is_empty() {
            validate_path(&device.platform, &path)?;
        }
        self.connector
            .directories(
                actor.principal().id,
                device,
                peri_acp_types::device_executor::DirectoryQuery {
                    path,
                    offset,
                    limit: 50,
                },
            )
            .await
    }

    pub async fn set_browser_instance_default(
        &self,
        actor: &Authenticated,
        id: Uuid,
        path: String,
    ) -> GatewayResult<String> {
        if path.is_empty() {
            return Err(GatewayError::Invalid);
        }
        let listing = self
            .browse_browser_instance(actor, id, Some(path), 0)
            .await?;
        self.identity.browser_scope(actor).await?;
        self.identity
            .set_instance_default(actor.principal().id, id, &listing.path)
            .await?;
        Ok(listing.path)
    }
    pub(crate) async fn select_instance(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
        device: &DeviceRecord,
        selected: Option<Uuid>,
        workspace: Option<&str>,
    ) -> GatewayResult<FrozenSession> {
        let path = workspace.unwrap_or(&device.default_workspace);
        validate_path(&device.platform, path)?;
        if let Some(old) = selected {
            let state = self.runtime.journal().session(principal, old).await?;
            if state.frozen.device_id == Some(device.id) {
                return self
                    .change_workspace(route, principal, device, old, path)
                    .await;
            }
            if !state.execution_settled()
                || self.runtime.journal().has_unconfirmed_turn(old).await?
            {
                return Err(GatewayError::Busy);
            }
        }
        let agent = self
            .connector
            .open_workspace(principal, device, path)
            .await?;
        let frozen = agent.frozen_session(principal);
        self.attach(route, principal, device, agent).await?;
        if let Some(old) = selected {
            self.inherit_model_selection(principal, old, frozen.session_id)
                .await?;
        }
        self.store
            .select(route, principal, frozen.session_id)
            .await?;
        Ok(frozen)
    }
    /// Only current, still-linked chat contexts belonging to the browser account.
    pub async fn browser_workspaces(
        &self,
        actor: &Authenticated,
    ) -> GatewayResult<Vec<WorkspaceSelection>> {
        self.identity.browser_scope(actor).await?;
        let gate = self.account_gate(actor.principal().id.to_string());
        let _guard = gate.lock().await;
        let principal = actor.principal().id;
        let mut result = Vec::new();
        for (route, session) in self.store.selections(principal).await? {
            let (owner, devices) = match self.identity.channel_devices(&route.identity).await {
                Ok(value) => value,
                Err(IdentityError::Unauthorized | IdentityError::Forbidden) => continue,
                Err(error) => return Err(error.into()),
            };
            if owner != principal {
                continue;
            }
            let state = self.runtime.journal().session(principal, session).await?;
            let Some(device) = devices
                .iter()
                .find(|d| Some(d.id) == state.frozen.device_id)
            else {
                continue;
            };
            // Only executor-verified bindings are saved choices. Registration
            // may use Windows short paths or aliases for the same directory.
            let paths = self.store.known_workspaces(principal, device.id).await?;
            let busy = !state.execution_settled()
                || self.runtime.journal().has_unconfirmed_turn(session).await?;
            result.push(WorkspaceSelection {
                session_id: session,
                device_id: device.id,
                device_name: device.name.clone(),
                platform: device.platform.clone(),
                adapter: route.identity.adapter_instance_id,
                conversation_id: route.conversation_id,
                workspace: state.frozen.workspace(),
                default_workspace: device.default_workspace.clone(),
                instance_kind: if device.is_local {
                    "local"
                } else {
                    "connected"
                }
                .into(),
                known_workspaces: paths,
                busy,
            });
        }
        Ok(result)
    }

    /// The deployment owns this mutation even if the HTTP caller disconnects.
    /// Exact current session plus the receive gate prevents stale page races.
    pub async fn switch_browser_workspace(
        self: &Arc<Self>,
        actor: &Authenticated,
        expected: Uuid,
        workspace: String,
    ) -> GatewayResult<Uuid> {
        self.identity.browser_scope(actor).await?;
        self.reap().await?;
        let (sender, receiver) = oneshot::channel();
        {
            let mut owner = self.owner.lock();
            if owner.closing {
                return Err(GatewayError::Closing);
            }
            if owner.tasks.len() >= 128 {
                return Err(GatewayError::Busy);
            }
            let gateway = self.clone();
            let actor = actor.clone();
            owner.tasks.push(tokio::spawn(async move {
                let result = gateway
                    .switch_browser_owned(&actor, expected, &workspace)
                    .await;
                let uncertain = matches!(
                    &result,
                    Err(GatewayError::Database(_)
                        | GatewayError::Encoding(_)
                        | GatewayError::RecoveryRequired)
                );
                let _ = sender.send(result);
                if uncertain {
                    Err(GatewayError::RecoveryRequired)
                } else {
                    Ok(())
                }
            }));
        }
        receiver.await.map_err(|_| GatewayError::RecoveryRequired)?
    }

    async fn switch_browser_owned(
        &self,
        actor: &Authenticated,
        expected: Uuid,
        workspace: &str,
    ) -> GatewayResult<Uuid> {
        let gate = self.account_gate(actor.principal().id.to_string());
        let _guard = gate.lock().await;
        self.identity.browser_scope(actor).await?;
        let principal = actor.principal().id;
        let (route, _) = self
            .store
            .selections(principal)
            .await?
            .into_iter()
            .find(|(_, id)| *id == expected)
            .ok_or(GatewayError::StaleInteraction)?;
        let (owner, devices) = self.identity.channel_devices(&route.identity).await?;
        if owner != principal {
            return Err(IdentityError::Forbidden.into());
        }
        let state = self.runtime.journal().session(principal, expected).await?;
        let device = devices
            .iter()
            .find(|d| Some(d.id) == state.frozen.device_id)
            .ok_or(IdentityError::Forbidden)?;
        let workspace = if workspace.is_empty() {
            &device.default_workspace
        } else {
            workspace
        };
        Ok(self
            .change_workspace(&route, principal, device, expected, workspace)
            .await?
            .session_id)
    }

    pub(super) async fn change_workspace(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
        device: &DeviceRecord,
        expected: Uuid,
        workspace: &str,
    ) -> GatewayResult<FrozenSession> {
        validate_path(&device.platform, workspace)?;
        if self.store.selection(route, principal).await? != Some(expected) {
            return Err(GatewayError::StaleInteraction);
        }
        let state = self.runtime.journal().session(principal, expected).await?;
        if !state.execution_settled()
            || self
                .runtime
                .journal()
                .has_unconfirmed_turn(expected)
                .await?
        {
            return Err(GatewayError::Busy);
        }
        if workspace == state.frozen.workspace() {
            return Ok(state.frozen);
        }
        let agent = self
            .connector
            .open_workspace(principal, device, workspace)
            .await?;
        let frozen = agent.frozen_session(principal);
        // The executor canonicalizes the directory on its own OS before selection.
        self.attach(route, principal, device, agent).await?;
        let mode: PermissionMode = state.permissions()?;
        self.runtime
            .journal()
            .set_permissions(principal, frozen.session_id, mode)
            .await?;
        self.inherit_model_selection(principal, expected, frozen.session_id)
            .await?;
        self.store
            .select(route, principal, frozen.session_id)
            .await?;
        Ok(frozen)
    }
    pub(super) async fn inherit_model_selection(
        &self,
        principal: Uuid,
        from: Uuid,
        to: Uuid,
    ) -> GatewayResult<()> {
        if let Some(aggregate) = self
            .connector
            .account_models()
            .and_then(|m| m.aggregation())
        {
            aggregate
                .inherit_selection(principal, from, to)
                .await
                .map_err(|_| GatewayError::RecoveryRequired)?;
        }
        Ok(())
    }
}

pub(super) fn connection_arguments(input: &str) -> GatewayResult<(Uuid, Option<&str>)> {
    let input = input.trim();
    let (id, path) = input
        .split_once(char::is_whitespace)
        .map_or((input, None), |(id, path)| (id, Some(path.trim())));
    Ok((
        Uuid::parse_str(id).map_err(|_| GatewayError::Invalid)?,
        path.filter(|path| !path.is_empty()),
    ))
}

pub(super) fn validate_path(platform: &str, path: &str) -> GatewayResult<()> {
    let bytes = path.as_bytes();
    let absolute = match platform {
        "windows" => {
            (bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && matches!(bytes[2], b'\\' | b'/'))
                || (path.starts_with("\\\\")
                    && path[2..]
                        .split('\\')
                        .filter(|part| !part.is_empty())
                        .count()
                        >= 2)
        }
        "linux" => path.starts_with('/'),
        _ => false,
    };
    if !absolute || path.len() > 4096 || path.chars().any(char::is_control) {
        return Err(GatewayError::Invalid);
    }
    Ok(())
}
