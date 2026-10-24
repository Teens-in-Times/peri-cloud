//! Shared task admission; HTTP controls never become fabricated platform events.
use super::*;
use crate::identity::Authenticated;
use crate::state::{SessionState, TurnRecord};
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Serialize)]
pub struct TaskReceipt {
    pub session_id: Uuid,
    pub turn_id: Uuid,
    pub state: crate::state::TurnState,
}
impl From<TurnRecord> for TaskReceipt {
    fn from(value: TurnRecord) -> Self {
        Self {
            session_id: value.session_id,
            turn_id: value.turn_id,
            state: value.state,
        }
    }
}
impl Gateway {
    pub(super) async fn ensure_task_session(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
        device: &DeviceRecord,
        state: &SessionState,
    ) -> GatewayResult<()> {
        let session = state.frozen.session_id;
        if !state.execution_settled()
            || self.runtime.journal().has_unconfirmed_turn(session).await?
        {
            return Err(GatewayError::Busy);
        }
        if !self.origins.read().contains_key(&session) || !self.runtime.has_device_session(session)
        {
            let agent = match self
                .connector
                .open(principal, device, Some(&state.frozen))
                .await
            {
                Ok(agent) => {
                    self.attach(route, principal, device, agent).await?;
                    return Ok(());
                }
                Err(GatewayError::Connection | GatewayError::NotConfigured) => {
                    self.connector
                        .open_conversation(principal, &state.frozen)
                        .await?
                }
                Err(error) => return Err(error),
            };
            self.attach_conversation(route, principal, agent).await?;
        }
        Ok(())
    }
    pub(super) async fn submit_session_task(
        &self,
        principal: Uuid,
        session: Uuid,
        request_key: String,
        prompt: MessageContent,
    ) -> GatewayResult<TurnRecord> {
        Ok(self
            .runtime
            .submit(crate::SubmitTurn {
                principal_id: principal,
                session_id: session,
                request_key,
                prompt,
                max_iterations: self.max_iterations,
            })
            .await?)
    }
    pub async fn submit_browser_task(
        self: &Arc<Self>,
        actor: &Authenticated,
        session: Uuid,
        request: Uuid,
        prompt: String,
    ) -> GatewayResult<TaskReceipt> {
        self.identity.browser_scope(actor).await?;
        if prompt.trim().starts_with('/')
            && !self
                .connector
                .command_registry()
                .is_some_and(|registry| registry.resolve(prompt.trim()).is_some())
        {
            return Err(GatewayError::Invalid);
        }
        if prompt.trim().is_empty() || prompt.len() > self.limits().max_prompt_bytes {
            return Err(GatewayError::Invalid);
        }
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
                    .submit_browser_owned(&actor, session, request, prompt)
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
    async fn submit_browser_owned(
        &self,
        actor: &Authenticated,
        session: Uuid,
        request: Uuid,
        prompt: String,
    ) -> GatewayResult<TaskReceipt> {
        let principal = actor.principal().id;
        let gate = self.account_gate(principal.to_string());
        let _guard = loop {
            if self.owner.lock().closing {
                return Err(GatewayError::Closing);
            }
            let guard = gate.lock().await;
            self.identity.browser_scope(actor).await?;
            let active = self.journal().active_turn(principal, session).await?;
            if (active.is_none() && self.runtime.status().active_sessions.contains(&session))
                || active
                    .as_ref()
                    .is_some_and(crate::tasks::approval_inbox::is_control_turn)
            {
                // This owned submission survives a disconnected HTTP waiter.
                // Let the bounded control turn finish without holding its gate.
                drop(guard);
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                continue;
            }
            break guard;
        };
        self.identity.browser_scope(actor).await?;
        let route = self.browser_task_route(actor, session).await?;
        let key = format!("web:{request}");
        let content = MessageContent::Text(prompt.clone());
        if let Some(existing) = self
            .journal()
            .existing_request(principal, session, &key, &content)
            .await?
        {
            return Ok(existing.into());
        }
        let fingerprint = (Sha256::digest(serde_json::to_vec(&(session, prompt.as_str()))?))
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let existing = sqlx::query_as::<_, (String, String)>(
            "SELECT principal,request_hash FROM control_operations WHERE id=?",
        )
        .bind(request.to_string())
        .fetch_optional(&self.store.pool)
        .await?;
        if let Some((owner, hash)) = existing {
            if owner != principal.to_string() || hash != fingerprint {
                return Err(StateError::InvocationConflict.into());
            }
            return Err(GatewayError::RecoveryRequired);
        }
        let (_, devices) = self.identity.channel_devices(&route.identity).await?;
        let state = self.journal().session(principal, session).await?;
        if state.frozen.device_id.is_none() {
            if !state.execution_settled() || self.journal().has_unconfirmed_turn(session).await? {
                return Err(GatewayError::Busy);
            }
            let agent = self
                .connector
                .open_conversation(principal, &state.frozen)
                .await?;
            self.attach_conversation(&route, principal, agent).await?;
        } else {
            let device = devices
                .iter()
                .find(|d| Some(d.id) == state.frozen.device_id)
                .ok_or(IdentityError::Forbidden)?;
            self.ensure_task_session(&route, principal, device, &state)
                .await?;
        }
        sqlx::query("INSERT INTO control_operations(id,principal,kind,target,request_hash,state,result_json,created_at) VALUES(?,?,'task.submit',?,?,'pending',?,?)")
            .bind(request.to_string()).bind(principal.to_string()).bind(session.to_string()).bind(fingerprint)
            .bind(serde_json::json!({"route_json":serde_json::to_string(&route)?}).to_string()).bind(self.identity.now()).execute(&self.store.pool).await?;
        let origin = self
            .origins
            .read()
            .get(&session)
            .cloned()
            .ok_or(GatewayError::RecoveryRequired)?;
        *origin.write() = Some(SessionOrigin::Browser);
        let result = self
            .submit_session_task(principal, session, key, content)
            .await;
        sqlx::query("UPDATE control_operations SET state=? WHERE id=?")
            .bind(if result.is_ok() {
                "succeeded"
            } else {
                "unknown"
            })
            .bind(request.to_string())
            .execute(&self.store.pool)
            .await?;
        result.map(Into::into)
    }
    pub(super) async fn browser_task_route(
        &self,
        actor: &Authenticated,
        session: Uuid,
    ) -> GatewayResult<ChannelRoute> {
        self.identity.browser_scope(actor).await?;
        let principal = actor.principal().id;
        let (route, _) = self
            .store
            .selections(principal)
            .await?
            .into_iter()
            .find(|(_, id)| *id == session)
            .ok_or(GatewayError::StaleInteraction)?;
        if !self.route_allowed(&route).await?
            || self.identity.resolve_channel(&route.identity).await? != principal
        {
            return Err(IdentityError::Forbidden.into());
        }
        Ok(route)
    }
    pub async fn open_browser_workspace(
        self: &Arc<Self>,
        actor: &Authenticated,
        request: Uuid,
        device_id: Uuid,
        workspace: Option<String>,
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
                    .open_browser_workspace_owned(&actor, request, device_id, workspace)
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
    async fn open_browser_workspace_owned(
        &self,
        actor: &Authenticated,
        request: Uuid,
        device_id: Uuid,
        workspace: Option<String>,
    ) -> GatewayResult<Uuid> {
        let principal = actor.principal().id;
        let gate = self.account_gate(principal.to_string());
        let _guard = gate.lock().await;
        let identity = self.identity.web_identity(actor).await?;
        if !self.adapters.contains_key(crate::gateway::WEB_ADAPTER) {
            return Err(GatewayError::Invalid);
        }
        let route = ChannelRoute {
            identity,
            conversation_id: format!("workspace:{request}"),
        };
        if !self.route_allowed(&route).await? {
            return Err(IdentityError::Forbidden.into());
        }
        let devices = self.identity.devices(actor).await?;
        let device = devices
            .iter()
            .find(|d| d.id == device_id && !d.revoked)
            .ok_or(IdentityError::Forbidden)?;
        if let Some(session) = self.store.selection(&route, principal).await? {
            let state = self.journal().session(principal, session).await?;
            if state.frozen.device_id != Some(device_id) {
                return Err(StateError::InvocationConflict.into());
            }
            if workspace
                .as_ref()
                .is_some_and(|w| w != &state.frozen.workspace())
            {
                return Err(StateError::InvocationConflict.into());
            }
            return Ok(session);
        }
        let frozen = self
            .select_instance(&route, principal, device, None, workspace.as_deref())
            .await?;
        Ok(frozen.session_id)
    }
    pub async fn cancel_browser_task(
        &self,
        actor: &Authenticated,
        session: Uuid,
        turn: Uuid,
    ) -> GatewayResult<TaskReceipt> {
        self.browser_task_route(actor, session).await?;
        Ok(self
            .runtime
            .cancel(actor.principal().id, session, turn)
            .await?
            .into())
    }
}
