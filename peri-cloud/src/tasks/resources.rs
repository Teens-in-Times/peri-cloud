//! Conservative device mutation ownership. Chat remains responsive while a worker owns a device.
use super::*;
use async_trait::async_trait;
use peri_agent::tools::{BaseTool, ContextRetention, ToolContext, ToolOutput};
use sqlx::Row;
use std::sync::Arc;

impl WorkerService {
    pub(crate) async fn claim_device(
        &self,
        principal: Uuid,
        session: Uuid,
        device: Uuid,
        reserved: bool,
    ) -> Result<()> {
        let state = self
            .journal
            .session(principal, session)
            .await
            .map_err(|_| TaskError::Forbidden)?;
        if state.frozen.device_id != Some(device) {
            return Err(TaskError::Forbidden);
        }
        let mut tx = self.store.pool.begin().await?;
        let row = sqlx::query(
            "SELECT owner_session,phase FROM agent_device_leases WHERE device_id=? AND principal=?",
        )
        .bind(device.to_string())
        .bind(principal.to_string())
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = row {
            let owner: String = row.get("owner_session");
            if owner == session.to_string() {
                tx.commit().await?;
                return Ok(());
            }
            if row.get::<String, _>("phase") == "reserved" {
                let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_tasks WHERE worker_session=? AND json_extract(record_json,'$.state') NOT IN ('completed','failed','cancelled','recovery_required','invalidated')")
                    .bind(&owner).fetch_one(&mut *tx).await?;
                if pending != 0 {
                    return Err(TaskError::Conflict);
                }
            }
            let raw: String = sqlx::query_scalar(
                "SELECT state_json FROM cloud_sessions WHERE id=? AND principal=?",
            )
            .bind(&owner)
            .bind(principal.to_string())
            .fetch_one(&mut *tx)
            .await?;
            let previous: crate::state::SessionState = serde_json::from_str(&raw)?;
            let active:i64=sqlx::query_scalar("SELECT COUNT(*) FROM cloud_turns WHERE session_id=? AND state IN ('queued','running','recovery_required')").bind(&owner).fetch_one(&mut *tx).await?;
            if active > 0 || !previous.execution_settled() {
                return Err(TaskError::Conflict);
            }
            sqlx::query("DELETE FROM agent_device_leases WHERE device_id=? AND owner_session=?")
                .bind(device.to_string())
                .bind(owner)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("INSERT INTO agent_device_leases(device_id,principal,owner_session,phase) VALUES(?,?,?,?)").bind(device.to_string()).bind(principal.to_string()).bind(session.to_string()).bind(if reserved {"reserved"} else {"active"}).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
    pub(crate) async fn release_settled(&self, principal: Uuid, session: Uuid) -> Result<()> {
        let state = self
            .journal
            .session(principal, session)
            .await
            .map_err(|_| TaskError::Forbidden)?;
        if state.execution_settled()
            && !self
                .journal
                .has_unconfirmed_turn(session)
                .await
                .map_err(|_| TaskError::Unavailable)?
        {
            sqlx::query("DELETE FROM agent_device_leases WHERE principal=? AND owner_session=?")
                .bind(principal.to_string())
                .bind(session.to_string())
                .execute(&self.store.pool)
                .await?;
        }
        Ok(())
    }
}
pub(crate) struct ResourceTool {
    pub inner: Arc<dyn BaseTool>,
    pub scope: TaskScope,
    pub device: Uuid,
}
impl ResourceTool {
    async fn acquire(&self) -> Result<()> {
        let service = self.scope.service.upgrade().ok_or(TaskError::Unavailable)?;
        if !service
            .gateway
            .upgrade()
            .ok_or(TaskError::Unavailable)?
            .worker_scope_valid(&self.scope)
            .await
            .map_err(|_| TaskError::Unavailable)?
        {
            return Err(TaskError::Forbidden);
        }
        if let Some(task) = self.scope.task {
            service.authorized(&self.scope, task).await?;
        }
        service
            .claim_device(self.scope.principal, self.scope.session, self.device, false)
            .await
    }
}
#[async_trait]
impl BaseTool for ResourceTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters(&self) -> serde_json::Value {
        self.inner.parameters()
    }
    fn is_direct(&self) -> bool {
        self.inner.is_direct()
    }
    fn aliases(&self) -> &[&str] {
        self.inner.aliases()
    }
    fn namespace(&self) -> Option<&str> {
        self.inner.namespace()
    }
    fn timeout(&self) -> Option<std::time::Duration> {
        self.inner.timeout()
    }
    fn output_char_limit(&self) -> Option<usize> {
        self.inner.output_char_limit()
    }
    fn prefers_persist(&self) -> bool {
        self.inner.prefers_persist()
    }
    fn context_retention(&self) -> ContextRetention {
        self.inner.context_retention()
    }
    fn prompt_declaration(&self) -> Option<String> {
        self.inner.prompt_declaration()
    }
    async fn invoke(
        &self,
        input: serde_json::Value,
        ctx: ToolContext<'_>,
    ) -> std::result::Result<String, Box<dyn std::error::Error + Send + Sync>> {
        self.acquire().await?;
        self.inner.invoke(input, ctx).await
    }
    async fn invoke_output(
        &self,
        input: serde_json::Value,
        ctx: ToolContext<'_>,
    ) -> std::result::Result<ToolOutput, Box<dyn std::error::Error + Send + Sync>> {
        self.acquire().await?;
        self.inner.invoke_output(input, ctx).await
    }
}
