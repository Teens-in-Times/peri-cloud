//! IM uses the same deterministic aggregation selection service as the portal.
use super::Gateway;
use crate::aggregation::{ControlActor, ModelSelection};
use crate::gateway::menu_store::{MenuAction, MenuRecord};
use crate::gateway::{
    DeliveryBody, GatewayError, GatewayResult, InboundMessage, MenuButton, MenuCard,
};
use uuid::Uuid;
impl Gateway {
    pub(super) async fn aggregate_model_menu(
        &self,
        message: &InboundMessage,
        principal: Uuid,
        selected: Option<Uuid>,
        page: usize,
    ) -> GatewayResult<MenuCard> {
        let aggregate = self
            .connector
            .account_models()
            .and_then(|m| m.aggregation())
            .ok_or(GatewayError::Invalid)?;
        let actor = ControlActor::Channel(&message.route.identity);
        let selection = aggregate
            .selection(&actor, selected)
            .await
            .map_err(|_| GatewayError::Connection)?;
        let current = aggregate
            .current_model(principal, selected)
            .await
            .map_err(|_| GatewayError::Connection)?;
        let models = aggregate
            .models(&actor)
            .await
            .map_err(|_| GatewayError::Connection)?;
        let pages = models.len().div_ceil(8).max(1);
        if page == 0 || page > pages {
            return Err(GatewayError::Invalid);
        }
        let mut options: Vec<(String, MenuAction)> = models
            .iter()
            .skip((page - 1) * 8)
            .take(8)
            .map(|model| {
                (
                    model.clone(),
                    MenuAction::ChooseAggregateModel {
                        model: Some(model.clone()),
                        revision: selection.revision,
                    },
                )
            })
            .collect();
        options.push((
            "使用默认模型".into(),
            MenuAction::ChooseAggregateModel {
                model: None,
                revision: selection.revision,
            },
        ));
        if page > 1 {
            options.push((
                "上一页".into(),
                MenuAction::AggregateModels { page: page - 1 },
            ));
        }
        if page < pages {
            options.push((
                "下一页".into(),
                MenuAction::AggregateModels { page: page + 1 },
            ));
        }
        let mut buttons = Vec::new();
        for (label, action) in options {
            let id = self
                .store
                .save_menu_action(
                    &message.route,
                    principal,
                    MenuRecord {
                        action,
                        expected_session: selected,
                        expires_at: self.identity.now() + 240,
                    },
                )
                .await?;
            buttons.push(MenuButton {
                label,
                action_id: id,
            });
        }
        Ok(MenuCard{title:format!("统一模型 · {page}/{pages}"),text:format!("当前模型：{current}\n出口：New API\n范围：{}\n选择从下一轮生效，当前任务继续使用原模型。\n也可发送 /模型 模型名称；/模型 默认 恢复默认。",if selected.is_some(){"当前会话"}else{"账号默认"}),buttons,question_request:None})
    }
    pub(super) async fn choose_aggregate_model(
        &self,
        message: &InboundMessage,
        selected: Option<Uuid>,
        model: Option<String>,
        revision: i64,
    ) -> GatewayResult<DeliveryBody> {
        let aggregate = self
            .connector
            .account_models()
            .and_then(|m| m.aggregation())
            .ok_or(GatewayError::Invalid)?;
        aggregate
            .choose_model(
                &ControlActor::Channel(&message.route.identity),
                selected,
                ModelSelection { model, revision },
            )
            .await
            .map_err(|_| GatewayError::Invalid)?;
        Ok("模型选择已保存，下一轮生效。当前任务继续使用原模型。".into())
    }
}
