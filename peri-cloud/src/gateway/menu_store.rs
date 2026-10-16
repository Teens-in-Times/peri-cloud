use super::{
    store::GatewayStore, ChannelRoute, Delivery, DeliveryBody, GatewayError, GatewayResult,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum MenuAction {
    Access {
        adapter: Option<String>,
        section: String,
        page: usize,
    },
    AccessChange {
        adapter: String,
        revision: u64,
        change: super::access::AccessChange,
    },
    AccessInput {
        adapter: String,
        revision: u64,
        list: super::access::AccessList,
    },
    Home,
    Personal {
        section: String,
        page: usize,
    },
    ClearPersonal {
        field: String,
        revision: u64,
    },
    Instances {
        offset: usize,
    },
    Workspaces,
    WorkspacesPage {
        offset: usize,
    },
    Browse {
        device: Uuid,
        path: Option<String>,
        offset: usize,
    },
    Select {
        device: Uuid,
        workspace: Option<String>,
    },
    SetDefault {
        device: Uuid,
        workspace: String,
    },
    Permissions,
    SetPermission {
        mode: String,
    },
    Status,
    Results {
        turn: Option<Uuid>,
        page: usize,
    },
    Cancel,
    QuestionPage {
        request: Uuid,
        revision: u32,
        question: usize,
        offset: usize,
    },
    QuestionDecline {
        request: Uuid,
        revision: u32,
    },
    QuestionOption {
        request: Uuid,
        revision: u32,
        question: usize,
        option: usize,
        selected: bool,
    },
    QuestionNext {
        request: Uuid,
        revision: u32,
        question: usize,
    },
}

impl MenuAction {
    pub fn mutates(&self) -> bool {
        matches!(
            self,
            Self::AccessChange { .. }
                | Self::AccessInput { .. }
                | Self::Select { .. }
                | Self::ClearPersonal { .. }
                | Self::SetDefault { .. }
                | Self::SetPermission { .. }
                | Self::Cancel
                | Self::QuestionOption { .. }
                | Self::QuestionNext { .. }
                | Self::QuestionPage { .. }
                | Self::QuestionDecline { .. }
        )
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct MenuRecord {
    pub action: MenuAction,
    pub expected_session: Option<Uuid>,
    pub expires_at: i64,
}

impl GatewayStore {
    pub(super) async fn menu_snapshot(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
        snapshot: Option<Uuid>,
    ) -> GatewayResult<Option<Delivery>> {
        // The immutable outbox ID pins all pages to the same menu. A later
        // menu cannot replace its names, and another route cannot read it.
        let rows = sqlx::query(concat!(
            "SELECT delivery_json FROM gateway_outbox WHERE principal=? ",
            "AND state IN ('delivered','unconfirmed') ",
            "AND json_extract(delivery_json,'$.body.kind')='menu' ",
            "AND json_extract(delivery_json,'$.route.identity.adapter_instance_id')=? ",
            "AND json_extract(delivery_json,'$.route.identity.external_user_id')=? ",
            "AND json_extract(delivery_json,'$.route.conversation_id')=? ",
            "AND (? IS NULL OR json_extract(delivery_json,'$.delivery_id')=?) ",
            "ORDER BY rowid DESC LIMIT 1"
        ))
        .bind(principal.to_string())
        .bind(&route.identity.adapter_instance_id)
        .bind(&route.identity.external_user_id)
        .bind(&route.conversation_id)
        .bind(snapshot.map(|id| id.to_string()))
        .bind(snapshot.map(|id| id.to_string()))
        .fetch_all(&self.pool)
        .await?;
        for row in rows {
            let delivery: Delivery = serde_json::from_str(row.get("delivery_json"))?;
            if delivery.route == *route && matches!(delivery.body, DeliveryBody::Menu { .. }) {
                return Ok(Some(delivery));
            }
        }
        Ok(None)
    }

    pub(super) async fn menu_number(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
        index: usize,
    ) -> GatewayResult<Uuid> {
        if !(1..=15).contains(&index) {
            return Err(GatewayError::Invalid);
        }
        let rows=sqlx::query("SELECT delivery_json FROM gateway_outbox WHERE principal=? AND state='delivered' ORDER BY rowid DESC LIMIT 64")
            .bind(principal.to_string()).fetch_all(&self.pool).await?;
        for row in rows {
            let delivery: Delivery = serde_json::from_str(row.get("delivery_json"))?;
            if delivery.route == *route {
                if let DeliveryBody::Menu { card } = delivery.body {
                    return card
                        .buttons
                        .get(index - 1)
                        .map(|button| button.action_id)
                        .ok_or(GatewayError::Invalid);
                }
                if matches!(delivery.body, DeliveryBody::Reply { .. }) {
                    break;
                }
            }
        }
        Err(GatewayError::StaleInteraction)
    }
    pub(super) async fn snapshot_number(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
        snapshot: Uuid,
        index: usize,
    ) -> GatewayResult<Uuid> {
        if !(1..=15).contains(&index) {
            return Err(GatewayError::Invalid);
        }
        let delivery = self
            .menu_snapshot(route, principal, Some(snapshot))
            .await?
            .ok_or(GatewayError::StaleInteraction)?;
        let DeliveryBody::Menu { card } = delivery.body else {
            return Err(GatewayError::Invalid);
        };
        card.buttons
            .get(index - 1)
            .map(|button| button.action_id)
            .ok_or(GatewayError::Invalid)
    }

    pub(super) async fn save_menu_action(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
        record: MenuRecord,
    ) -> GatewayResult<Uuid> {
        let id = Uuid::new_v4();
        // Keep expired actions for one day for delayed callback notices, then
        // discard them. Live menus remain valid across a service restart.
        sqlx::query("DELETE FROM gateway_menu_actions WHERE expires_at<?")
            .bind(record.expires_at - 86400 - 240)
            .execute(&self.pool)
            .await?;
        sqlx::query("INSERT INTO gateway_menu_actions(id,route_key,principal,record_json,expires_at) VALUES(?,?,?,?,?)")
            .bind(id.to_string()).bind(route.key()?).bind(principal.to_string()).bind(serde_json::to_string(&record)?).bind(record.expires_at).execute(&self.pool).await?;
        Ok(id)
    }

    pub(super) async fn menu_action(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
        id: Uuid,
        now: i64,
    ) -> GatewayResult<MenuRecord> {
        let row = sqlx::query("SELECT record_json FROM gateway_menu_actions WHERE id=? AND route_key=? AND principal=? AND expires_at>?")
            .bind(id.to_string()).bind(route.key()?).bind(principal.to_string()).bind(now).fetch_optional(&self.pool).await?.ok_or(GatewayError::StaleInteraction)?;
        Ok(serde_json::from_str(row.get("record_json"))?)
    }

    pub(super) async fn consume_menu_action(&self, id: Uuid, event: &str) -> GatewayResult<()> {
        let result = sqlx::query(
            "UPDATE gateway_menu_actions SET used_event=? WHERE id=? AND used_event IS NULL",
        )
        .bind(event)
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        if result.rows_affected() != 1 {
            return Err(GatewayError::StaleInteraction);
        }
        Ok(())
    }
}
