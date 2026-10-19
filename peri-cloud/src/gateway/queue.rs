//! Durable FIFO per logical conversation. Message transport is not conversation identity.
use super::*;
use crate::identity::IdentityError;
use crate::state::StateError;
use sqlx::Row;
use uuid::Uuid;

impl GatewayStore {
    pub(super) async fn recover_queue(&self) -> GatewayResult<()> {
        // No tool can execute before the original cloud turn is committed. A
        // missing turn after exclusive-owner recovery proves no admission.
        let rows =
            sqlx::query("SELECT event_key,session_id FROM gateway_queue WHERE state='dispatching'")
                .fetch_all(&self.pool)
                .await?;
        for row in rows {
            let key: String = row.get("event_key");
            let turn: Option<String> = sqlx::query_scalar(
                "SELECT id FROM cloud_turns WHERE session_id=? AND request_key IN (?,?)",
            )
            .bind(row.get::<String, _>("session_id"))
            .bind(format!("gateway:{key}"))
            .bind(format!("task-event:{key}"))
            .fetch_optional(&self.pool)
            .await?;
            let mut tx = self.pool.begin().await?;
            sqlx::query("UPDATE gateway_queue SET state=? WHERE event_key=?")
                .bind(if turn.is_some() {
                    "submitted"
                } else {
                    "queued"
                })
                .bind(&key)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE gateway_inbox SET state=?,turn_id=? WHERE event_key=?")
                .bind(if turn.is_some() {
                    "submitted"
                } else {
                    "queued"
                })
                .bind(turn)
                .bind(&key)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
        Ok(())
    }
}
impl Gateway {
    pub(super) async fn enqueue_session_message(
        &self,
        message: &InboundMessage,
        principal: Uuid,
        session: Uuid,
    ) -> GatewayResult<()> {
        self.enqueue_with_origin(
            message,
            principal,
            session,
            false,
            &message.event_id,
            message.reply_kind,
        )
        .await
    }
    pub(super) async fn enqueue_with_origin(
        &self,
        message: &InboundMessage,
        principal: Uuid,
        session: Uuid,
        worker_event: bool,
        reply_reference: &str,
        reply_kind: ReplyKind,
    ) -> GatewayResult<()> {
        let mut tx = self.store.pool.begin().await?;
        let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM gateway_queue WHERE principal=? AND state IN ('queued','dispatching')").bind(principal.to_string()).fetch_one(&mut *tx).await?;
        if pending >= 64 {
            return Err(StateError::Limit.into());
        }
        if !worker_event {
            let raw: String = sqlx::query_scalar(
                "SELECT state_json FROM cloud_sessions WHERE id=? AND principal=?",
            )
            .bind(session.to_string())
            .bind(principal.to_string())
            .fetch_one(&mut *tx)
            .await?;
            let state: crate::state::SessionState = serde_json::from_str(&raw)?;
            crate::state::chat_projection::user(
                &mut tx,
                &state,
                &format!("gateway:{}", message.key()?),
                &MessageContent::Text(message.text.clone()),
            )
            .await?;
        }
        sqlx::query("INSERT INTO gateway_queue(event_key,principal,session_id,message_json,source,reply_reference,reply_kind,state) VALUES(?,?,?,?,?,?,?,'queued')")
            .bind(message.key()?).bind(principal.to_string()).bind(session.to_string()).bind(serde_json::to_string(message)?).bind(if worker_event {"worker"} else {"user"}).bind(reply_reference).bind(serde_json::to_string(&reply_kind)?).execute(&mut *tx).await?;
        sqlx::query("UPDATE gateway_inbox SET principal=?,session_id=?,state='queued' WHERE event_key=? AND state='processing'")
            .bind(principal.to_string()).bind(session.to_string()).bind(message.key()?).execute(&mut *tx).await?;
        tx.commit().await?;
        self.drive_session_locked(principal, session).await
    }
    // Caller holds the authenticated account gate. Never changes a live turn's broker.
    async fn drive_session_locked(&self, principal: Uuid, session: Uuid) -> GatewayResult<()> {
        if self.runtime.status().active_sessions.contains(&session)
            || self.journal().has_unconfirmed_turn(session).await?
        {
            return Ok(());
        }
        let state = self.journal().session(principal, session).await?;
        if !state.execution_settled() {
            return Ok(());
        }
        let row = sqlx::query("SELECT event_key,message_json,source,reply_reference,reply_kind FROM gateway_queue WHERE principal=? AND session_id=? AND state='queued' ORDER BY seq LIMIT 1")
            .bind(principal.to_string()).bind(session.to_string()).fetch_optional(&self.store.pool).await?;
        let Some(row) = row else {
            return Ok(());
        };
        let key: String = row.get("event_key");
        let message: InboundMessage = serde_json::from_str(row.get("message_json"))?;
        let source: String = row.get("source");
        let resolved = match self.identity.resolve_channel(&message.route.identity).await {
            Ok(id) => Some(id),
            Err(IdentityError::Unauthorized | IdentityError::Forbidden) => None,
            Err(error) => return Err(error.into()),
        };
        let devices = crate::memory::scope::devices(&self.store.pool, principal).await?;
        let mut authorized = self.route_allowed(&message.route).await?
            && resolved == Some(principal)
            && state
                .frozen
                .device_id
                .is_none_or(|id| devices.iter().any(|d| d.id == id))
            && crate::memory::context::can_attach(
                self.journal(),
                principal,
                session,
                self.private_memory_route(&message.route),
            )
            .await?;
        if authorized && source == "worker" {
            let id = serde_json::from_str::<serde_json::Value>(&message.text)?
                .get("task_id")
                .and_then(|v| v.as_str())
                .and_then(|v| Uuid::parse_str(v).ok())
                .ok_or(GatewayError::Invalid)?;
            let scope = self
                .workers
                .scope(principal, session, message.route.clone())
                .await
                .map_err(|_| GatewayError::RecoveryRequired)?;
            authorized = self.workers.authorized(&scope, id).await.is_ok();
        }
        if !authorized {
            sqlx::query("UPDATE gateway_queue SET state='suppressed' WHERE event_key=?")
                .bind(&key)
                .execute(&self.store.pool)
                .await?;
            sqlx::query("UPDATE gateway_inbox SET state='interrupted' WHERE event_key=?")
                .bind(&key)
                .execute(&self.store.pool)
                .await?;
            return Ok(());
        }
        if let Some(device_id) = state.frozen.device_id {
            let device = devices
                .iter()
                .find(|d| d.id == device_id)
                .ok_or(IdentityError::Forbidden)?;
            // Reattach at the boundary, including an explicit cross-gateway resume.
            let agent = match self
                .connector
                .open(principal, device, Some(&state.frozen))
                .await
            {
                Ok(agent) => agent,
                Err(GatewayError::Connection | GatewayError::NotConfigured) => {
                    self.connector
                        .open_conversation(principal, &state.frozen)
                        .await?
                }
                Err(error) => return Err(error),
            };
            self.attach(&message.route, principal, device, agent)
                .await?;
        } else {
            let agent = self
                .connector
                .open_conversation(principal, &state.frozen)
                .await?;
            self.attach_conversation(&message.route, principal, agent)
                .await?;
        }
        let origin = self
            .origins
            .read()
            .get(&session)
            .cloned()
            .ok_or(GatewayError::RecoveryRequired)?;
        let reply_kind: ReplyKind = serde_json::from_str(row.get("reply_kind"))?;
        let reply_reference: String = row.get("reply_reference");
        *origin.write() = Some(if reply_kind == ReplyKind::Proactive {
            SessionOrigin::Scheduled(reply_reference)
        } else {
            SessionOrigin::Platform(reply_reference)
        });
        sqlx::query(
            "UPDATE gateway_queue SET state='dispatching' WHERE event_key=? AND state='queued'",
        )
        .bind(&key)
        .execute(&self.store.pool)
        .await?;
        let request_key = if source == "worker" {
            format!("task-event:{key}")
        } else {
            format!("gateway:{key}")
        };
        let submitted = self
            .submit_session_task(
                principal,
                session,
                request_key,
                peri_acp_types::messages::MessageContent::Text(message.text.clone()),
            )
            .await;
        match submitted {
            Ok(turn) => {
                self.store.submitted_queued(&key, turn.turn_id).await?;
                sqlx::query("UPDATE gateway_queue SET state='submitted' WHERE event_key=?")
                    .bind(key)
                    .execute(&self.store.pool)
                    .await?;
                Ok(())
            }
            Err(GatewayError::Runtime(crate::RuntimeError::State(
                StateError::Busy | StateError::Limit,
            ))) => {
                sqlx::query("UPDATE gateway_queue SET state='queued' WHERE event_key=?")
                    .bind(key)
                    .execute(&self.store.pool)
                    .await?;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
    pub(super) async fn drive_message_queue(&self) -> GatewayResult<()> {
        let rows = sqlx::query("SELECT principal,session_id FROM gateway_queue q WHERE state='queued' AND seq=(SELECT MIN(seq) FROM gateway_queue h WHERE h.session_id=q.session_id AND h.state='queued') ORDER BY seq LIMIT 64").fetch_all(&self.store.pool).await?;
        for row in rows {
            let principal =
                Uuid::parse_str(row.get("principal")).map_err(|_| GatewayError::Invalid)?;
            let session =
                Uuid::parse_str(row.get("session_id")).map_err(|_| GatewayError::Invalid)?;
            let gate = self.account_gate(principal.to_string());
            let _guard = gate.lock().await;
            match self.drive_session_locked(principal, session).await {
                Ok(()) | Err(GatewayError::Connection | GatewayError::NotConfigured) => (),
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}
impl GatewayStore {
    async fn submitted_queued(&self, key: &str, turn: Uuid) -> GatewayResult<()> {
        sqlx::query("UPDATE gateway_inbox SET state='submitted',turn_id=? WHERE event_key=? AND state='queued'")
            .bind(turn.to_string()).bind(key).execute(&self.pool).await?;
        Ok(())
    }
}
