use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use peri_acp_types::interaction::{
    ApprovalDecision, InteractionContext, InteractionResponse, UserInteractionBroker,
};
use tokio::sync::oneshot;
use uuid::Uuid;

use super::store::GatewayStore;
use super::{ChannelRoute, GatewayError, GatewayResult, InteractionAction, InteractionCard};
use crate::identity::IdentityService;
use crate::state::{CloudJournal, TurnState};

struct Pending {
    route: ChannelRoute,
    principal: Uuid,
    card: InteractionCard,
    response: oneshot::Sender<InteractionResponse>,
}

pub(crate) struct InteractionHub {
    identity: Arc<IdentityService>,
    journal: Arc<CloudJournal>,
    runtime: Weak<crate::CloudRuntime>,
    store: GatewayStore,
    pending: Mutex<HashMap<Uuid, Pending>>,
}

impl InteractionHub {
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
        for (route, card) in candidates {
            match self.validate_current(&route, principal, &card).await {
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
        let route = self
            .pending
            .lock()
            .get(&request)
            .filter(|pending| pending.principal == principal)
            .map(|pending| pending.route.clone())
            .ok_or(GatewayError::StaleInteraction)?;
        self.respond(&route, request, action).await
    }

    async fn validate_current(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
        card: &InteractionCard,
    ) -> GatewayResult<()> {
        let (resolved, _) = self
            .identity
            .channel_device(&route.identity, card.device_id)
            .await?;
        if principal != resolved
            || self.store.selection(route, principal).await? != Some(card.session_id)
        {
            return Err(GatewayError::StaleInteraction);
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

pub(crate) struct SessionBroker {
    pub hub: Arc<InteractionHub>,
    pub route: ChannelRoute,
    pub principal: Uuid,
    pub session: Uuid,
    pub device: Uuid,
    pub device_name: String,
    pub workspace: String,
    pub origin: Arc<RwLock<Option<String>>>,
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
    async fn request(&self, context: InteractionContext) -> InteractionResponse {
        let fallback = || unavailable(&context);
        let Some(parent) = self.origin.read().clone() else {
            return fallback();
        };
        if !valid_context(&context) {
            return fallback();
        }
        let Ok((principal, _)) = self
            .hub
            .identity
            .channel_device(&self.route.identity, self.device)
            .await
        else {
            return fallback();
        };
        if principal != self.principal {
            return fallback();
        }
        let Ok(Some(turn)) = self.hub.journal.active_turn(principal, self.session).await else {
            return fallback();
        };
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
        self.hub.pending.lock().insert(
            request,
            Pending {
                route: self.route.clone(),
                principal,
                card: card.clone(),
                response,
            },
        );
        let _guard = RequestGuard {
            hub: self.hub.clone(),
            request,
        };
        if self
            .hub
            .store
            .interaction(&self.route, &parent, principal, card)
            .await
            .is_err()
        {
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
        // No question tool is currently installed by CloudChainAssembler. A
        // future question-capable adapter must distinguish lifecycle cancellation
        // in the shared broker contract before enabling timed question requests.
        InteractionContext::Questions { .. } => InteractionResponse::Rejected,
    }
}
