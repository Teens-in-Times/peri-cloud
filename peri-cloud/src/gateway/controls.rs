//! Text alternatives use the same route-owned broker request as native buttons.
use super::*;

impl Gateway {
    pub(super) async fn interaction_command(
        &self,
        message: &InboundMessage,
        text: &str,
        principal: Uuid,
    ) -> GatewayResult<Option<DeliveryBody>> {
        let words: Vec<_> = text.split_whitespace().collect();
        let Some(verb) = words.first() else {
            return Ok(None);
        };
        if !matches!(*verb, "/允许" | "/允许一次" | "/拒绝" | "/详情") {
            return Ok(None);
        }
        if words.len() < 2 || words.len() > 3 {
            return Err(GatewayError::Invalid);
        }
        let card = self.hub.card_by_code(&message.route, words[1]).await?;
        if self
            .identity
            .channel_device(&message.route.identity, card.device_id)
            .await?
            .0
            != principal
        {
            return Err(IdentityError::Forbidden.into());
        }
        if *verb == "/详情" {
            let page = words
                .get(2)
                .map(|s| s.parse::<usize>().map_err(|_| GatewayError::Invalid))
                .transpose()?
                .unwrap_or(1);
            return Ok(Some(activity::interaction_details(&card, page)?.into()));
        }
        if words.len() != 2 {
            return Err(GatewayError::Invalid);
        }
        let action = if *verb == "/拒绝" {
            InteractionAction::Reject {}
        } else {
            InteractionAction::AllowOnce {}
        };
        self.hub
            .respond(&message.route, card.request_id, action)
            .await?;
        Ok(Some(
            if *verb == "/拒绝" {
                "已拒绝这次请求。"
            } else {
                "已允许卡片中的本次操作；之后的操作继续按当前权限审批。"
            }
            .into(),
        ))
    }
}
