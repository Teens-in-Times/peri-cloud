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
    pub known_workspaces: Vec<String>,
    pub busy: bool,
}

impl Gateway {
    /// Only current, still-linked chat contexts belonging to the browser account.
    pub async fn browser_workspaces(
        &self,
        actor: &Authenticated,
    ) -> GatewayResult<Vec<WorkspaceSelection>> {
        self.identity.browser_scope(actor).await?;
        let _gate = self.receive_gate.lock().await;
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
            let Some(device) = devices.iter().find(|d| d.id == state.frozen.device_id) else {
                continue;
            };
            let mut paths = self.store.known_workspaces(principal, device.id).await?;
            if !paths.iter().any(|path| {
                path_key(&device.platform, path)
                    == path_key(&device.platform, &device.default_workspace)
            }) {
                paths.push(device.default_workspace.clone());
            }
            let busy = !state.execution_settled()
                || self.runtime.journal().has_unconfirmed_turn(session).await?;
            result.push(WorkspaceSelection {
                session_id: session,
                device_id: device.id,
                device_name: device.name.clone(),
                platform: device.platform.clone(),
                adapter: route.identity.adapter_instance_id,
                conversation_id: route.conversation_id,
                workspace: state.frozen.binding.workspace,
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
        let _gate = self.receive_gate.lock().await;
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
            .find(|d| d.id == state.frozen.device_id)
            .ok_or(IdentityError::Forbidden)?;
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
        if workspace == state.frozen.binding.workspace {
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
        self.store
            .select(route, principal, frozen.session_id)
            .await?;
        Ok(frozen)
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

fn path_key(platform: &str, path: &str) -> String {
    if platform != "windows" {
        return path.to_owned();
    }
    let path = path.replace('/', "\\");
    if let Some(unc) = path.strip_prefix("\\\\?\\UNC\\") {
        return format!("\\\\{unc}");
    }
    path.strip_prefix("\\\\?\\").unwrap_or(&path).to_owned()
}
