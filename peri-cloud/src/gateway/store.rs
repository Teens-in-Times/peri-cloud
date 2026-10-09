use peri_acp_types::interaction::InteractionContext;
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use super::types::digest;
use super::{
    ChannelRoute, Delivery, DeliveryBody, GatewayError, GatewayReceipt, GatewayResult,
    InboundMessage, ReceiptState,
};
use crate::state::{CloudJournal, TurnRecord, TurnState};

pub(crate) async fn migrate(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let (version,): (i64,) = sqlx::query_as("PRAGMA user_version")
        .fetch_one(&mut *tx)
        .await?;
    if version == 2 {
        for statement in [
            "CREATE TABLE gateway_routes (route_key TEXT PRIMARY KEY, route_json TEXT NOT NULL, principal TEXT NOT NULL REFERENCES principals(id), session_id TEXT NOT NULL REFERENCES cloud_sessions(id))",
            "CREATE TABLE gateway_inbox (event_key TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, route_json TEXT NOT NULL, event_id TEXT NOT NULL, principal TEXT, session_id TEXT, turn_id TEXT, state TEXT NOT NULL)",
            "CREATE TABLE gateway_outbox (semantic_key TEXT PRIMARY KEY, delivery_json TEXT NOT NULL, reply_key TEXT NOT NULL, sequence INTEGER NOT NULL, principal TEXT, device_id TEXT, state TEXT NOT NULL, UNIQUE(reply_key,sequence))",
        ] { sqlx::query(statement).execute(&mut *tx).await?; }
        sqlx::query("PRAGMA user_version = 3")
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

#[derive(Clone)]
pub(crate) struct GatewayStore {
    pool: SqlitePool,
}

pub(crate) struct PendingTurn {
    pub event_key: String,
    pub route: ChannelRoute,
    pub event_id: String,
    pub principal: Uuid,
    pub session: Uuid,
    pub turn: Uuid,
}

pub(crate) struct PendingDelivery {
    pub key: String,
    pub delivery: Delivery,
    pub principal: Option<Uuid>,
    pub device: Option<Uuid>,
}

impl GatewayStore {
    pub fn new(journal: &CloudJournal) -> Self {
        Self {
            pool: journal.identity_pool(),
        }
    }

    /// Called once by a new deployment, never against a live gateway owner.
    pub async fn recover(&self) -> GatewayResult<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE gateway_outbox SET state='unconfirmed' WHERE state='sending'")
            .execute(&mut *tx)
            .await?;
        let rows =
            sqlx::query("SELECT event_key,session_id FROM gateway_inbox WHERE state='processing'")
                .fetch_all(&mut *tx)
                .await?;
        for row in rows {
            let key: String = row.get("event_key");
            let session: Option<String> = row.get("session_id");
            let turn =
                sqlx::query("SELECT id FROM cloud_turns WHERE session_id=? AND request_key=?")
                    .bind(session)
                    .bind(format!("gateway:{key}"))
                    .fetch_optional(&mut *tx)
                    .await?;
            sqlx::query("UPDATE gateway_inbox SET state=?,turn_id=? WHERE event_key=?")
                .bind(if turn.is_some() {
                    "submitted"
                } else {
                    "interrupted"
                })
                .bind(turn.map(|row| row.get::<String, _>("id")))
                .bind(key)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn admit(&self, message: &InboundMessage) -> GatewayResult<(GatewayReceipt, bool)> {
        let key = message.key()?;
        let fingerprint = message.fingerprint()?;
        let result = sqlx::query("INSERT OR IGNORE INTO gateway_inbox(event_key,fingerprint,route_json,event_id,state) VALUES(?,?,?,?,'processing')")
            .bind(&key).bind(&fingerprint).bind(serde_json::to_string(&message.route)?).bind(&message.event_id).execute(&self.pool).await?;
        let row = sqlx::query("SELECT fingerprint FROM gateway_inbox WHERE event_key=?")
            .bind(&key)
            .fetch_one(&self.pool)
            .await?;
        if row.get::<String, _>("fingerprint") != fingerprint {
            return Err(GatewayError::MessageConflict);
        }
        Ok((self.receipt(&key).await?, result.rows_affected() == 1))
    }

    pub async fn receipt(&self, key: &str) -> GatewayResult<GatewayReceipt> {
        let row =
            sqlx::query("SELECT state,session_id,turn_id FROM gateway_inbox WHERE event_key=?")
                .bind(key)
                .fetch_one(&self.pool)
                .await?;
        let state = match row.get::<&str, _>("state") {
            "processing" => ReceiptState::Processing,
            "submitted" => ReceiptState::Submitted,
            "completed" => ReceiptState::Completed,
            "interrupted" => ReceiptState::Interrupted,
            _ => return Err(GatewayError::RecoveryRequired),
        };
        Ok(GatewayReceipt {
            event_key: key.into(),
            state,
            session_id: optional_uuid(row.get("session_id"))?,
            turn_id: optional_uuid(row.get("turn_id"))?,
        })
    }

    pub async fn selection(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
    ) -> GatewayResult<Option<Uuid>> {
        let row = sqlx::query("SELECT principal,session_id FROM gateway_routes WHERE route_key=?")
            .bind(route.key()?)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| {
            if row.get::<String, _>("principal") != principal.to_string() {
                return Err(GatewayError::MessageConflict);
            }
            uuid(row.get("session_id"))
        })
        .transpose()
    }

    pub async fn select(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
        session: Uuid,
    ) -> GatewayResult<()> {
        sqlx::query("INSERT INTO gateway_routes(route_key,route_json,principal,session_id) VALUES(?,?,?,?) ON CONFLICT(route_key) DO UPDATE SET session_id=excluded.session_id")
            .bind(route.key()?).bind(serde_json::to_string(route)?).bind(principal.to_string()).bind(session.to_string()).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn stage(&self, key: &str, principal: Uuid, session: Uuid) -> GatewayResult<()> {
        sqlx::query("UPDATE gateway_inbox SET principal=?,session_id=? WHERE event_key=? AND state='processing'")
            .bind(principal.to_string()).bind(session.to_string()).bind(key).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn link_existing(&self, key: &str) -> GatewayResult<bool> {
        let row = sqlx::query("SELECT t.id FROM gateway_inbox i JOIN cloud_turns t ON t.session_id=i.session_id AND t.request_key=? WHERE i.event_key=? AND i.state='processing'")
            .bind(format!("gateway:{key}")).bind(key).fetch_optional(&self.pool).await?;
        if let Some(row) = row {
            self.submitted(key, uuid(row.get("id"))?).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub async fn submitted(&self, key: &str, turn: Uuid) -> GatewayResult<()> {
        sqlx::query("UPDATE gateway_inbox SET state='submitted',turn_id=? WHERE event_key=? AND state='processing'")
            .bind(turn.to_string()).bind(key).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn notice(
        &self,
        message: &InboundMessage,
        text: String,
        principal: Option<Uuid>,
    ) -> GatewayResult<()> {
        let mut tx = self.pool.begin().await?;
        enqueue(
            &mut tx,
            &format!("notice:{}", message.key()?),
            &message.route,
            &message.event_id,
            DeliveryBody::Notice { text },
            principal,
            None,
        )
        .await?;
        sqlx::query(
            "UPDATE gateway_inbox SET state='completed' WHERE event_key=? AND state='processing'",
        )
        .bind(message.key()?)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn interaction(
        &self,
        route: &ChannelRoute,
        parent: &str,
        principal: Uuid,
        card: super::InteractionCard,
    ) -> GatewayResult<()> {
        let mut tx = self.pool.begin().await?;
        enqueue(
            &mut tx,
            &format!("interaction:{}", card.request_id),
            route,
            parent,
            DeliveryBody::Interaction { card: card.clone() },
            Some(principal),
            Some(card.device_id),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn pending_turns(&self) -> GatewayResult<Vec<PendingTurn>> {
        let rows = sqlx::query("SELECT * FROM gateway_inbox WHERE state='submitted' LIMIT 128")
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok(PendingTurn {
                    event_key: row.get("event_key"),
                    route: serde_json::from_str(row.get("route_json"))?,
                    event_id: row.get("event_id"),
                    principal: uuid(row.get("principal"))?,
                    session: uuid(row.get("session_id"))?,
                    turn: uuid(row.get("turn_id"))?,
                })
            })
            .collect()
    }

    pub async fn project(
        &self,
        pending: &PendingTurn,
        turn: &TurnRecord,
        device: Uuid,
    ) -> GatewayResult<()> {
        if matches!(turn.state, TurnState::Queued | TurnState::Running) {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        for reply in &turn.replies {
            enqueue(
                &mut tx,
                &format!("reply:{}:{}", turn.turn_id, reply.message_id),
                &pending.route,
                &pending.event_id,
                DeliveryBody::Reply {
                    reply: reply.clone(),
                },
                Some(pending.principal),
                Some(device),
            )
            .await?;
        }
        let notice = match turn.state {
            TurnState::RecoveryRequired => Some("这次任务的执行状态需要恢复确认；没有重新执行。"),
            TurnState::Interrupted => {
                Some("这次任务已中断。仍需以电脑端任务状态确认是否全部停止。")
            }
            TurnState::Failed if turn.replies.is_empty() => {
                Some("这次任务未能完成，可以查看服务状态后重试。")
            }
            _ => None,
        };
        if let Some(text) = notice {
            enqueue(
                &mut tx,
                &format!("turn-status:{}", turn.turn_id),
                &pending.route,
                &pending.event_id,
                DeliveryBody::Notice { text: text.into() },
                Some(pending.principal),
                Some(device),
            )
            .await?;
        }
        sqlx::query(
            "UPDATE gateway_inbox SET state='completed' WHERE event_key=? AND state='submitted'",
        )
        .bind(&pending.event_key)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn ready(&self) -> GatewayResult<Vec<PendingDelivery>> {
        let rows = sqlx::query("SELECT semantic_key,delivery_json,principal,device_id FROM gateway_outbox WHERE state='ready' ORDER BY rowid LIMIT 128").fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|row| {
                Ok(PendingDelivery {
                    key: row.get("semantic_key"),
                    delivery: serde_json::from_str(row.get("delivery_json"))?,
                    principal: optional_uuid(row.get("principal"))?,
                    device: optional_uuid(row.get("device_id"))?,
                })
            })
            .collect()
    }

    pub async fn begin_send(&self, key: &str) -> GatewayResult<bool> {
        Ok(sqlx::query(
            "UPDATE gateway_outbox SET state='sending' WHERE semantic_key=? AND state='ready'",
        )
        .bind(key)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    pub async fn delivered(&self, key: &str, state: &str) -> GatewayResult<()> {
        sqlx::query("UPDATE gateway_outbox SET state=? WHERE semantic_key=? AND state IN ('ready','sending')").bind(state).bind(key).execute(&self.pool).await?;
        Ok(())
    }
}

async fn enqueue(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    key: &str,
    route: &ChannelRoute,
    parent: &str,
    body: DeliveryBody,
    principal: Option<Uuid>,
    device: Option<Uuid>,
) -> GatewayResult<()> {
    let (exists,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM gateway_outbox WHERE semantic_key=?")
            .bind(key)
            .fetch_one(&mut **tx)
            .await?;
    if exists != 0 {
        return Ok(());
    }
    let reply_key = digest(&serde_json::to_vec(&(route, parent))?);
    let (last,): (i64,) =
        sqlx::query_as("SELECT COALESCE(MAX(sequence),0) FROM gateway_outbox WHERE reply_key=?")
            .bind(&reply_key)
            .fetch_one(&mut **tx)
            .await?;
    let sequence = u32::try_from(last + 1).map_err(|_| GatewayError::Invalid)?;
    let delivery = Delivery {
        delivery_id: Uuid::new_v4(),
        route: route.clone(),
        in_reply_to: parent.into(),
        message_sequence: sequence,
        body,
    };
    // Context is bounded by the tool/request boundary; avoid unbounded DB cards.
    if let DeliveryBody::Interaction { card } = &delivery.body {
        if serde_json::to_vec(&card.context)?.len() > 64 * 1024 {
            return Err(GatewayError::Invalid);
        }
        match &card.context {
            InteractionContext::Approval { items } if items.is_empty() => {
                return Err(GatewayError::Invalid)
            }
            InteractionContext::Questions { requests } if requests.is_empty() => {
                return Err(GatewayError::Invalid)
            }
            _ => (),
        }
    }
    sqlx::query("INSERT INTO gateway_outbox(semantic_key,delivery_json,reply_key,sequence,principal,device_id,state) VALUES(?,?,?,?,?,?,'ready')")
        .bind(key).bind(serde_json::to_string(&delivery)?).bind(reply_key).bind(i64::from(sequence)).bind(principal.map(|id| id.to_string())).bind(device.map(|id| id.to_string())).execute(&mut **tx).await?;
    Ok(())
}

fn uuid(value: &str) -> GatewayResult<Uuid> {
    Uuid::parse_str(value).map_err(|_| GatewayError::RecoveryRequired)
}
fn optional_uuid(value: Option<String>) -> GatewayResult<Option<Uuid>> {
    value.as_deref().map(uuid).transpose()
}
