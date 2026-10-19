//! Attach at an idle turn boundary; live replies and approvals keep their origin.
use super::*;

impl Gateway {
    pub(super) async fn attach(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
        device: &DeviceRecord,
        agent: Arc<crate::CloudAgent>,
    ) -> GatewayResult<()> {
        self.attach_origin(route, principal, Some(device), agent)
            .await
    }
    pub(super) async fn attach_conversation(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
        agent: Arc<crate::CloudAgent>,
    ) -> GatewayResult<()> {
        self.attach_origin(route, principal, None, agent).await
    }
    pub(super) async fn attach_origin(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
        device: Option<&DeviceRecord>,
        agent: Arc<crate::CloudAgent>,
    ) -> GatewayResult<()> {
        let private = self.private_memory_route(route);
        let agent = agent.for_memory_visibility(private);
        let task_scope = self
            .workers
            .scope(
                principal,
                agent.frozen_session(principal).session_id,
                route.clone(),
            )
            .await
            .map_err(|_| GatewayError::RecoveryRequired)?;
        agent.set_task_scope(task_scope);
        agent.set_channel_constraints(
            self.adapters
                .get(&route.identity.adapter_instance_id)
                .map(|adapter| adapter.context_constraints(route))
                .unwrap_or_default()
                .to_owned(),
        );
        let frozen = agent.frozen_session(principal);
        if !crate::memory::context::can_attach(
            self.journal(),
            principal,
            frozen.session_id,
            private,
        )
        .await?
        {
            return Err(GatewayError::PrivateHistory);
        }
        agent.configure_schedule_notifications(
            self.adapters
                .get(&route.identity.adapter_instance_id)
                .is_some_and(|a| a.supports_proactive()),
        );
        let origin = Arc::new(RwLock::new(None));
        let broker = Arc::new(SessionBroker {
            hub: self.hub.clone(),
            route: route.clone(),
            principal,
            session: frozen.session_id,
            device: device.map(|d| d.id),
            device_name: device
                .map(|d| d.name.clone())
                .unwrap_or_else(|| "云端对话".into()),
            workspace: frozen.workspace(),
            origin: origin.clone(),
        });
        if self.origins.read().contains_key(&frozen.session_id) {
            self.runtime.rebind_idle(principal, agent, broker).await?;
        } else {
            self.runtime
                .attach(principal, agent, PermissionMode::Default, broker, None)
                .await?;
        }
        self.origins.write().insert(frozen.session_id, origin);
        crate::memory::context::record(self.journal(), frozen.session_id, private).await?;
        Ok(())
    }
}
