//! Explicit logical-session selection. OAuth authenticates; routes only select a session.
use super::{ChannelRoute, Gateway, GatewayError, GatewayResult, WEB_ADAPTER};
use crate::identity::{Authenticated, IdentityError};
use sha2::Digest;
use sqlx::Row;
use uuid::Uuid;

impl Gateway {
    pub async fn select_logical_conversation(
        &self,
        actor: &Authenticated,
        request: Uuid,
        target: Option<Uuid>,
    ) -> GatewayResult<Uuid> {
        self.identity.browser_scope(actor).await?;
        let principal = actor.principal().id;
        let digest = sha2::Sha256::digest(serde_json::to_vec(&(
            "peri-conversation-create-v1",
            principal,
            request,
        ))?);
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let new_session = Uuid::from_bytes(bytes);
        let gate = self.account_gate(principal.to_string());
        let _guard = gate.lock().await;
        self.identity.browser_scope(actor).await?;
        if !self.adapters.contains_key(WEB_ADAPTER) {
            return Err(GatewayError::Invalid);
        }
        let route = ChannelRoute {
            identity: self.identity.web_identity(actor).await?,
            conversation_id: super::web_chat::WEB_CONVERSATION.into(),
        };
        let fingerprint = format!(
            "{:x}",
            sha2::Sha256::digest(serde_json::to_vec(&("conversation.select", target))?)
        );
        if let Some(row) = sqlx::query(
            "SELECT principal,request_hash,state,result_json FROM control_operations WHERE id=?",
        )
        .bind(request.to_string())
        .fetch_optional(&self.store.pool)
        .await?
        {
            if row.get::<String, _>("principal") != principal.to_string()
                || row.get::<String, _>("request_hash") != fingerprint
            {
                return Err(GatewayError::MessageConflict);
            }
            if row.get::<String, _>("state") == "succeeded" {
                return serde_json::from_str(row.get("result_json")).map_err(GatewayError::from);
            }
        } else {
            sqlx::query("INSERT INTO control_operations(id,principal,kind,target,request_hash,state,result_json,created_at) VALUES(?,?,'conversation.select',?,?,'pending','null',?)")
                .bind(request.to_string()).bind(principal.to_string()).bind(target.unwrap_or(new_session).to_string()).bind(&fingerprint).bind(self.identity.now()).execute(&self.store.pool).await?;
        }
        let id = match target {
            Some(id) => {
                self.journal().session(principal, id).await?;
                if !crate::memory::context::can_attach(self.journal(), principal, id, true).await? {
                    return Err(GatewayError::PrivateHistory);
                }
                let worker: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM agent_execution_windows WHERE cloud_session=?",
                )
                .bind(id.to_string())
                .fetch_one(&self.store.pool)
                .await?;
                if worker != 0 {
                    return Err(IdentityError::Forbidden.into());
                }
                id
            }
            None => {
                match self.journal().session(principal, new_session).await {
                    Ok(_) => (),
                    Err(crate::state::StateError::NotFound) => {
                        let template = self.connector.new_conversation(principal).await?;
                        let mut frozen = template.frozen_session(principal);
                        frozen.session_id = new_session;
                        self.journal()
                            .bind(frozen, peri_acp_types::permission::PermissionMode::Default)
                            .await?;
                    }
                    Err(error) => return Err(error.into()),
                }
                crate::memory::context::record(self.journal(), new_session, true).await?;
                new_session
            }
        };
        let mut tx = self.store.pool.begin().await?;
        sqlx::query("INSERT INTO gateway_routes(route_key,route_json,principal,session_id) VALUES(?,?,?,?) ON CONFLICT(route_key) DO UPDATE SET session_id=excluded.session_id WHERE gateway_routes.principal=excluded.principal")
            .bind(route.key()?).bind(serde_json::to_string(&route)?).bind(principal.to_string()).bind(id.to_string()).execute(&mut *tx).await?;
        sqlx::query("UPDATE control_operations SET state='succeeded',result_json=? WHERE id=? AND principal=? AND request_hash=?")
            .bind(serde_json::to_string(&id)?).bind(request.to_string()).bind(principal.to_string()).bind(fingerprint).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(id)
    }
}
