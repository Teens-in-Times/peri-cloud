use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use peri_acp_types::interaction::{
    ApprovalDecision, InteractionContext, InteractionResponse, TrustedInteractionOwner,
    UserInteractionBroker,
};
use tokio::sync::oneshot;
use uuid::Uuid;

use super::store::GatewayStore;
use super::{ChannelRoute, GatewayError, GatewayResult, InteractionAction, InteractionCard};
use crate::identity::IdentityService;
use crate::state::{CloudJournal, TurnState};

#[path = "questions.rs"]
mod questions;

struct Pending {
    route: ChannelRoute,
    principal: Uuid,
    card: InteractionCard,
    response: oneshot::Sender<InteractionResponse>,
    draft: Option<questions::QuestionDraft>,
}

pub(crate) struct InteractionHub {
    identity: Arc<IdentityService>,
    journal: Arc<CloudJournal>,
    runtime: Weak<crate::CloudRuntime>,
    store: GatewayStore,
    pending: Mutex<HashMap<Uuid, Pending>>,
}

impl InteractionHub {
    pub(crate) async fn card_by_code(
        &self,
        route: &ChannelRoute,
        code: &str,
    ) -> GatewayResult<InteractionCard> {
        if !(8..=32).contains(&code.len()) || !code.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(GatewayError::Invalid);
        }
        let candidates: Vec<_> = self
            .pending
            .lock()
            .values()
            .filter(|p| {
                p.route == *route
                    && p.card
                        .request_id
                        .simple()
                        .to_string()
                        .starts_with(&code.to_ascii_lowercase())
                    && p.card.expires_at > self.identity.now()
                    && !p.response.is_closed()
            })
            .map(|p| (p.principal, p.card.clone()))
            .collect();
        let [(principal, card)] = candidates.as_slice() else {
            return Err(GatewayError::StaleInteraction);
        };
        self.validate_current(route, *principal, card).await?;
        Ok(card.clone())
    }
    pub fn new(
        identity: Arc<IdentityService>,
        journal: Arc<CloudJournal>,
        runtime: Weak<crate::CloudRuntime>,
        store: GatewayStore,
    ) -> Arc<Self> {
        Arc::new(Self {
            identity,
            journal,
            runtime,
            store,
            pending: Mutex::new(HashMap::new()),
        })
    }

    pub fn is_live(&self, request: Uuid) -> bool {
        self.pending.lock().get(&request).is_some_and(|pending| {
            pending.card.expires_at > self.identity.now() && !pending.response.is_closed()
        })
    }

    pub fn invalidate(&self, request: Uuid) {
        self.pending.lock().remove(&request);
    }

    pub async fn browser_cards(&self, principal: Uuid) -> GatewayResult<Vec<InteractionCard>> {
        let candidates: Vec<_> = self
            .pending
            .lock()
            .values()
            .filter(|pending| {
                pending.principal == principal
                    && pending.card.expires_at > self.identity.now()
                    && !pending.response.is_closed()
            })
            .map(|pending| (pending.route.clone(), pending.card.clone()))
            .collect();
        let mut cards = Vec::new();
        for (_, card) in candidates {
            match self.validate_principal_current(principal, &card).await {
                Ok(()) => cards.push(card),
                Err(
                    GatewayError::StaleInteraction
                    | GatewayError::Identity(
                        crate::identity::IdentityError::Forbidden
                        | crate::identity::IdentityError::Unauthorized,
                    ),
                ) => (),
                Err(error) => return Err(error),
            }
        }
        cards.sort_by_key(|card| (card.expires_at, card.request_id));
        Ok(cards)
    }

    pub async fn respond_browser(
        &self,
        principal: Uuid,
        request: Uuid,
        action: InteractionAction,
    ) -> GatewayResult<()> {
        let card = self
            .pending
            .lock()
            .get(&request)
            .filter(|pending| pending.principal == principal)
            .filter(|pending| !pending.response.is_closed())
            .map(|pending| pending.card.clone())
            .ok_or(GatewayError::StaleInteraction)?;
        self.validate_principal_current(principal, &card).await?;
        let response = action_response(&card.context, action, super::WEB_ADAPTER)?;
        self.complete(request, response)
    }

    async fn validate_principal_current(
        &self,
        principal: Uuid,
        card: &InteractionCard,
    ) -> GatewayResult<()> {
        if card.expires_at <= self.identity.now() {
            return Err(GatewayError::StaleInteraction);
        }
        let session = self.journal.session(principal, card.session_id).await?;
        if session.frozen.device_id != card.device_id {
            return Err(GatewayError::StaleInteraction);
        }
        if let Some(device) = card.device_id {
            if !crate::memory::scope::devices(&self.journal.identity_pool(), principal)
                .await?
                .iter()
                .any(|d| d.id == device)
            {
                return Err(GatewayError::StaleInteraction);
            }
        }
        let active = self.journal.active_turn(principal, card.session_id).await?;
        if !active.is_some_and(|turn| {
            turn.turn_id == card.turn_id
                && turn.state == TurnState::Running
                && !turn.cancel_requested
        }) {
            return Err(GatewayError::StaleInteraction);
        }
        Ok(())
    }

    async fn validate_current(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
        card: &InteractionCard,
    ) -> GatewayResult<()> {
        let resolved = match card.device_id {
            Some(device) => {
                self.identity
                    .channel_device(&route.identity, device)
                    .await?
                    .0
            }
            None => self.identity.resolve_channel(&route.identity).await?,
        };
        let selected = self.store.selection(route, principal).await?;
        let owns_selection = if selected == Some(card.session_id) {
            true
        } else {
            let parent: Option<String> = sqlx::query_scalar(
                "SELECT parent_session FROM agent_tasks WHERE principal=? AND worker_session=?",
            )
            .bind(principal.to_string())
            .bind(card.session_id.to_string())
            .fetch_optional(&self.journal.identity_pool())
            .await?;
            parent
                .and_then(|id| Uuid::parse_str(&id).ok())
                .is_some_and(|id| Some(id) == selected)
        };
        if principal != resolved || !owns_selection {
            return Err(GatewayError::StaleInteraction);
        }
        self.validate_principal_current(principal, card).await
    }

    pub async fn respond(
        &self,
        route: &ChannelRoute,
        request: Uuid,
        action: InteractionAction,
    ) -> GatewayResult<()> {
        route.key()?;
        let (principal, card) = {
            let pending = self.pending.lock();
            let pending = pending
                .get(&request)
                .ok_or(GatewayError::StaleInteraction)?;
            if pending.route != *route
                || pending.card.expires_at <= self.identity.now()
                || pending.response.is_closed()
            {
                return Err(GatewayError::StaleInteraction);
            }
            (pending.principal, pending.card.clone())
        };
        self.validate_current(route, principal, &card).await?;
        let response = action_response(&card.context, action, &route.identity.adapter_instance_id)?;
        self.complete(request, response)
    }

    fn complete(&self, request: Uuid, response: InteractionResponse) -> GatewayResult<()> {
        let mut pending = self.pending.lock();
        // Recheck expiry after authentication/database awaits, then consume once.
        if !pending
            .get(&request)
            .is_some_and(|p| p.card.expires_at > self.identity.now() && !p.response.is_closed())
        {
            return Err(GatewayError::StaleInteraction);
        }
        pending
            .remove(&request)
            .ok_or(GatewayError::StaleInteraction)?
            .response
            .send(response)
            .map_err(|_| GatewayError::StaleInteraction)
    }
}

#[derive(Clone)]
pub(crate) enum SessionOrigin {
    Platform(String),
    Scheduled(String),
    Browser,
}

pub(crate) struct SessionBroker {
    pub hub: Arc<InteractionHub>,
    pub route: ChannelRoute,
    pub principal: Uuid,
    pub session: Uuid,
    pub device: Option<Uuid>,
    pub device_name: String,
    pub workspace: String,
    pub origin: Arc<RwLock<Option<SessionOrigin>>>,
}

struct RequestGuard {
    hub: Arc<InteractionHub>,
    request: Uuid,
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.hub.pending.lock().remove(&self.request);
    }
}

#[async_trait]
impl UserInteractionBroker for SessionBroker {
    async fn request(
        &self,
        owner: TrustedInteractionOwner,
        context: InteractionContext,
    ) -> InteractionResponse {
        let fallback = || unavailable(&context);
        let Some((owner_session, owner_turn, _)) = owner.model_turn() else {
            return fallback();
        };
        if owner_session != self.session.to_string() {
            return fallback();
        }
        let Some(origin) = self.origin.read().clone() else {
            return fallback();
        };
        if !valid_context(&context) {
            return fallback();
        }
        let principal = self.principal;
        let Ok(Some(turn)) = self.hub.journal.active_turn(principal, self.session).await else {
            return fallback();
        };
        if owner_turn.as_uuid() != turn.turn_id {
            return fallback();
        }
        if turn.state != TurnState::Running || turn.cancel_requested {
            return fallback();
        }
        let Some(cancellation) =
            self.hub.runtime.upgrade().and_then(|runtime| {
                runtime.turn_cancellation(principal, self.session, turn.turn_id)
            })
        else {
            return fallback();
        };
        let request = Uuid::new_v4();
        let (response, receiver) = oneshot::channel();
        let card = InteractionCard {
            request_id: request,
            turn_id: turn.turn_id,
            session_id: self.session,
            device_id: self.device,
            device_name: self.device_name.clone(),
            workspace: self.workspace.clone(),
            expires_at: self.hub.identity.now().saturating_add(300),
            context: context.clone(),
        };
        if self
            .hub
            .validate_principal_current(principal, &card)
            .await
            .is_err()
        {
            return fallback();
        }
        self.hub.pending.lock().insert(
            request,
            Pending {
                route: self.route.clone(),
                principal,
                card: card.clone(),
                response,
                draft: questions::QuestionDraft::new(&context),
            },
        );
        let _guard = RequestGuard {
            hub: self.hub.clone(),
            request,
        };
        let delivery = match origin {
            SessionOrigin::Browser => Ok(()),
            SessionOrigin::Scheduled(parent) => {
                async {
                    let mut tx = self.hub.store.pool.begin().await?;
                    super::store::enqueue(
                        &mut tx,
                        &format!("interaction:{request}"),
                        &self.route,
                        (&parent, super::ReplyKind::Proactive),
                        super::DeliveryBody::Interaction { card },
                        Some(principal),
                        self.device,
                    )
                    .await?;
                    tx.commit().await?;
                    Ok(())
                }
                .await
            }
            SessionOrigin::Platform(parent) => {
                if let Some(device) = self
                    .device
                    .filter(|_| matches!(context, InteractionContext::Questions { .. }))
                {
                    self.hub
                        .deliver_question(&self.route, &parent, principal, request, device)
                        .await
                } else {
                    self.hub
                        .store
                        .interaction(&self.route, &parent, principal, card)
                        .await
                }
            }
        };
        if delivery.is_err() {
            return fallback();
        }
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => fallback(),
            result = tokio::time::timeout(Duration::from_secs(300), receiver) => match result {
                Ok(Ok(response)) => response,
                _ => fallback(),
            },
        }
    }
}

fn valid_context(context: &InteractionContext) -> bool {
    let ids: Vec<&str> = match context {
        InteractionContext::Approval { items } => items
            .iter()
            .map(|item| item.tool_call_id.as_str())
            .collect(),
        InteractionContext::Questions { requests } => {
            requests.iter().map(|item| item.id.as_str()).collect()
        }
    };
    !ids.is_empty()
        && ids.len() <= 64
        && ids.iter().all(|id| !id.is_empty() && id.len() <= 256)
        && ids.iter().copied().collect::<HashSet<_>>().len() == ids.len()
}

fn action_response(
    context: &InteractionContext,
    action: InteractionAction,
    source: &str,
) -> GatewayResult<InteractionResponse> {
    match (context, action) {
        (InteractionContext::Questions { requests }, InteractionAction::Choose { option })
            if requests.len() == 1 && !requests[0].multi_select =>
        {
            let question = &requests[0];
            let selected = question.options.get(option).ok_or(GatewayError::Invalid)?;
            action_response(
                context,
                InteractionAction::Answers {
                    answers: vec![peri_acp_types::interaction::QuestionAnswer {
                        id: question.id.clone(),
                        selected: vec![selected.label.clone()],
                        text: None,
                    }],
                },
                source,
            )
        }
        (InteractionContext::Approval { items }, InteractionAction::AllowOnce {}) => {
            Ok(InteractionResponse::Decisions(
                items
                    .iter()
                    .map(|_| ApprovalDecision::Approve {
                        source: Some(source.into()),
                    })
                    .collect(),
            ))
        }
        (InteractionContext::Approval { items }, InteractionAction::Reject {}) => {
            Ok(InteractionResponse::Decisions(
                items
                    .iter()
                    .map(|_| ApprovalDecision::Reject {
                        reason: "用户拒绝本次执行".into(),
                        source: Some(source.into()),
                    })
                    .collect(),
            ))
        }
        (InteractionContext::Questions { .. }, InteractionAction::Reject {}) => {
            Ok(InteractionResponse::Rejected)
        }
        (InteractionContext::Questions { requests }, InteractionAction::Answers { answers }) => {
            if answers.len() != requests.len() {
                return Err(GatewayError::Invalid);
            }
            let mut ordered = Vec::new();
            for question in requests {
                let mut matching = answers.iter().filter(|answer| answer.id == question.id);
                let answer = matching.next().ok_or(GatewayError::Invalid)?;
                if matching.next().is_some()
                    || (!question.multi_select && answer.selected.len() > 1)
                    || answer.selected.iter().collect::<HashSet<_>>().len() != answer.selected.len()
                    || answer.selected.iter().any(|selected| {
                        !question
                            .options
                            .iter()
                            .any(|option| option.label == *selected)
                    })
                    || answer.text.as_ref().is_some_and(|text| text.len() > 4096)
                    || (answer.selected.is_empty()
                        && answer
                            .text
                            .as_ref()
                            .is_none_or(|text| text.trim().is_empty()))
                {
                    return Err(GatewayError::Invalid);
                }
                ordered.push(answer.clone());
            }
            Ok(InteractionResponse::Answers(ordered))
        }
        _ => Err(GatewayError::Invalid),
    }
}

fn unavailable(context: &InteractionContext) -> InteractionResponse {
    match context {
        InteractionContext::Approval { items } => InteractionResponse::Decisions(
            items
                .iter()
                .map(|_| ApprovalDecision::Reject {
                    reason: "审批已过期或交互通道不可用；没有获得本次授权".into(),
                    source: None,
                })
                .collect(),
        ),
        InteractionContext::Questions { .. } => InteractionResponse::Interrupted {
            reason: "审批已过期或交互通道不可用；没有获得本次答复".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unavailable_question_is_lifecycle_cancellation_not_user_refusal() {
        assert!(matches!(
            unavailable(&InteractionContext::Questions {
                requests: Vec::new()
            }),
            InteractionResponse::Interrupted { .. }
        ));
    }
}
