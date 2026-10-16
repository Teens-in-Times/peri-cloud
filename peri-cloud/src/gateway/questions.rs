//! A question remains one broker request while QQ walks its fields. Opaque,
//! single-use menu actions and draft revisions reject duplicate/stale clicks.
use super::*;
use crate::gateway::menu_store::{MenuAction, MenuRecord};
use crate::gateway::{MenuButton, MenuCard};
use peri_acp_types::interaction::QuestionAnswer;

#[derive(Clone)]
pub(super) struct QuestionDraft {
    index: usize,
    revision: u32,
    offset: usize,
    answers: Vec<QuestionAnswer>,
}

impl QuestionDraft {
    pub fn new(context: &InteractionContext) -> Option<Self> {
        let InteractionContext::Questions { requests } = context else {
            return None;
        };
        Some(Self {
            index: 0,
            revision: 0,
            offset: 0,
            answers: requests
                .iter()
                .map(|q| QuestionAnswer {
                    id: q.id.clone(),
                    selected: Vec::new(),
                    text: None,
                })
                .collect(),
        })
    }
}

impl InteractionHub {
    pub(crate) fn question_is_current(&self, request: Uuid, revision: u32) -> bool {
        self.pending.lock().get(&request).is_some_and(|p| {
            p.card.expires_at > self.identity.now()
                && !p.response.is_closed()
                && p.draft.as_ref().is_some_and(|d| d.revision == revision)
        })
    }
    pub(crate) async fn question_delivery_current(
        &self,
        route: &ChannelRoute,
        request: Uuid,
        revision: u32,
    ) -> GatewayResult<bool> {
        if !self.question_is_current(request, revision) {
            return Ok(false);
        }
        match self.owned_question(route, request).await {
            Ok((_, _, draft)) => Ok(draft.revision == revision),
            Err(
                GatewayError::StaleInteraction
                | GatewayError::Identity(
                    crate::identity::IdentityError::Unauthorized
                    | crate::identity::IdentityError::Forbidden,
                ),
            ) => Ok(false),
            Err(error) => Err(error),
        }
    }
    async fn owned_question(
        &self,
        route: &ChannelRoute,
        request: Uuid,
    ) -> GatewayResult<(Uuid, InteractionCard, QuestionDraft)> {
        let (principal, card, draft) = {
            let pending = self.pending.lock();
            let pending = pending
                .get(&request)
                .filter(|p| {
                    p.route == *route
                        && p.card.expires_at > self.identity.now()
                        && !p.response.is_closed()
                })
                .ok_or(GatewayError::StaleInteraction)?;
            (
                pending.principal,
                pending.card.clone(),
                pending.draft.clone().ok_or(GatewayError::Invalid)?,
            )
        };
        self.validate_current(route, principal, &card).await?;
        Ok((principal, card, draft))
    }

    pub(crate) async fn current_question(
        &self,
        route: &ChannelRoute,
    ) -> GatewayResult<Option<Uuid>> {
        let ids: Vec<_> = self
            .pending
            .lock()
            .iter()
            .filter(|(_, p)| {
                p.route == *route
                    && p.draft.is_some()
                    && p.card.expires_at > self.identity.now()
                    && !p.response.is_closed()
            })
            .map(|(id, _)| *id)
            .collect();
        match ids.as_slice() {
            [] => Ok(None),
            [id] => {
                self.owned_question(route, *id).await?;
                Ok(Some(*id))
            }
            _ => Err(GatewayError::Busy),
        }
    }

    pub(crate) async fn deliver_question(
        &self,
        route: &ChannelRoute,
        parent: &str,
        principal: Uuid,
        request: Uuid,
        device: Uuid,
    ) -> GatewayResult<()> {
        let menu = self.question_menu(route, request).await?;
        self.store
            .question(route, parent, principal, request, device, menu)
            .await
    }

    pub(crate) async fn question_menu(
        &self,
        route: &ChannelRoute,
        request: Uuid,
    ) -> GatewayResult<MenuCard> {
        let (principal, card, draft) = self.owned_question(route, request).await?;
        let InteractionContext::Questions { requests } = &card.context else {
            return Err(GatewayError::Invalid);
        };
        let question = &requests[draft.index];
        let answer = &draft.answers[draft.index];
        let mut text = format!(
            "执行设备：{}\n{}\n\n{}\n{}",
            card.device_name,
            card.workspace,
            question.question,
            if question.multi_select {
                "可选择多项，选好后点击“提交本题”。"
            } else {
                "点击一个选项即可继续，也可直接回复文字。"
            }
        );
        let mut actions = Vec::new();
        for (index, option) in question
            .options
            .iter()
            .enumerate()
            .skip(draft.offset)
            .take(6)
        {
            let selected = answer.selected.contains(&option.label);
            text.push_str(&format!(
                "\n答案 {} · {}{}",
                index + 1,
                if selected { "✓ " } else { "" },
                option.label
            ));
            if let Some(description) = &option.description {
                text.push_str(&format!(" · {description}"));
            }
            actions.push((
                format!("{}{}", if selected { "✓ " } else { "" }, option.label),
                MenuAction::QuestionOption {
                    request,
                    revision: draft.revision,
                    question: draft.index,
                    option: index,
                    selected: !selected,
                },
            ));
        }
        for (label, offset) in [
            ("上一页选项", draft.offset.checked_sub(6)),
            (
                "下一页选项",
                (draft.offset + 6 < question.options.len()).then_some(draft.offset + 6),
            ),
        ] {
            if let Some(offset) = offset {
                actions.push((
                    label.into(),
                    MenuAction::QuestionPage {
                        request,
                        revision: draft.revision,
                        question: draft.index,
                        offset,
                    },
                ));
            }
        }
        if question.multi_select {
            actions.push((
                "提交本题".into(),
                MenuAction::QuestionNext {
                    request,
                    revision: draft.revision,
                    question: draft.index,
                },
            ));
        }
        if answer.text.is_some() {
            text.push_str("\n已填写文字答案。");
        }
        let code = &request.simple().to_string()[..8];
        text.push_str(&format!("\n\n回复 /回答 内容；多选回复 /回答 1,2。发送 /问题 重开本题。完整题目与选项：/详情 {code}。"));
        actions.push((
            "暂不回答".into(),
            MenuAction::QuestionDecline {
                request,
                revision: draft.revision,
            },
        ));
        let mut buttons = Vec::new();
        for (label, action) in actions {
            let id = self
                .store
                .save_menu_action(
                    route,
                    principal,
                    MenuRecord {
                        action,
                        expected_session: Some(card.session_id),
                        expires_at: card.expires_at,
                    },
                )
                .await?;
            buttons.push(MenuButton {
                label,
                action_id: id,
            });
        }
        Ok(MenuCard {
            title: format!("请回答 · {}/{}", draft.index + 1, requests.len()),
            text,
            buttons,
            question_request: Some((request, draft.revision)),
        })
    }

    pub(crate) async fn question_page(
        &self,
        route: &ChannelRoute,
        request: Uuid,
        revision: u32,
        question: usize,
        offset: usize,
    ) -> GatewayResult<MenuCard> {
        let (_, card, _) = self.owned_question(route, request).await?;
        let InteractionContext::Questions { requests } = &card.context else {
            return Err(GatewayError::Invalid);
        };
        if !offset.is_multiple_of(6)
            || offset
                >= requests
                    .get(question)
                    .ok_or(GatewayError::Invalid)?
                    .options
                    .len()
        {
            return Err(GatewayError::Invalid);
        }
        {
            let mut pending = self.pending.lock();
            let p = pending
                .get_mut(&request)
                .ok_or(GatewayError::StaleInteraction)?;
            let draft = p.draft.as_mut().ok_or(GatewayError::Invalid)?;
            if draft.revision != revision
                || draft.index != question
                || p.response.is_closed()
                || p.card.expires_at <= self.identity.now()
            {
                return Err(GatewayError::StaleInteraction);
            }
            draft.offset = offset;
            draft.revision = draft.revision.checked_add(1).ok_or(GatewayError::Invalid)?;
        }
        self.question_menu(route, request).await
    }

    pub(crate) async fn question_decline(
        &self,
        route: &ChannelRoute,
        request: Uuid,
        revision: u32,
    ) -> GatewayResult<MenuCard> {
        self.owned_question(route, request).await?;
        {
            let mut pending = self.pending.lock();
            let p = pending
                .get(&request)
                .ok_or(GatewayError::StaleInteraction)?;
            if p.draft.as_ref().is_none_or(|d| d.revision != revision)
                || p.response.is_closed()
                || p.card.expires_at <= self.identity.now()
            {
                return Err(GatewayError::StaleInteraction);
            }
            let response = action_response(
                &p.card.context,
                InteractionAction::Reject {},
                &route.identity.adapter_instance_id,
            )?;
            pending
                .remove(&request)
                .ok_or(GatewayError::StaleInteraction)?
                .response
                .send(response)
                .map_err(|_| GatewayError::StaleInteraction)?;
        }
        Ok(MenuCard {
            title: "已暂不回答".into(),
            text: "已告知 Agent 这次没有提供答案。可发送 /取消 停止任务。".into(),
            buttons: Vec::new(),
            question_request: None,
        })
    }

    pub(crate) async fn question_option(
        &self,
        route: &ChannelRoute,
        request: Uuid,
        revision: u32,
        question: usize,
        option: usize,
        selected: bool,
    ) -> GatewayResult<MenuCard> {
        let (_, card, _) = self.owned_question(route, request).await?;
        let InteractionContext::Questions { requests } = &card.context else {
            return Err(GatewayError::Invalid);
        };
        let item = requests.get(question).ok_or(GatewayError::Invalid)?;
        let label = item
            .options
            .get(option)
            .ok_or(GatewayError::Invalid)?
            .label
            .clone();
        {
            let mut pending = self.pending.lock();
            let p = pending
                .get_mut(&request)
                .ok_or(GatewayError::StaleInteraction)?;
            let draft = p.draft.as_mut().ok_or(GatewayError::Invalid)?;
            if draft.revision != revision
                || draft.index != question
                || p.card.expires_at <= self.identity.now()
                || p.response.is_closed()
            {
                return Err(GatewayError::StaleInteraction);
            }
            let answer = &mut draft.answers[question];
            if item.multi_select {
                answer.selected.retain(|s| *s != label);
                if selected {
                    answer.selected.push(label)
                }
            } else {
                if !selected {
                    return Err(GatewayError::Invalid);
                }
                answer.selected = vec![label];
            }
            draft.revision = draft.revision.checked_add(1).ok_or(GatewayError::Invalid)?;
        }
        if !item.multi_select {
            return self
                .question_next(route, request, revision + 1, question)
                .await;
        }
        self.question_menu(route, request).await
    }

    pub(crate) async fn question_next(
        &self,
        route: &ChannelRoute,
        request: Uuid,
        revision: u32,
        question: usize,
    ) -> GatewayResult<MenuCard> {
        self.owned_question(route, request).await?;
        let complete = {
            let mut pending = self.pending.lock();
            let p = pending
                .get_mut(&request)
                .ok_or(GatewayError::StaleInteraction)?;
            let draft = p.draft.as_mut().ok_or(GatewayError::Invalid)?;
            if draft.revision != revision
                || draft.index != question
                || p.card.expires_at <= self.identity.now()
                || p.response.is_closed()
            {
                return Err(GatewayError::StaleInteraction);
            }
            let answer = &draft.answers[question];
            if answer.selected.is_empty()
                && answer.text.as_ref().is_none_or(|s| s.trim().is_empty())
            {
                return Err(GatewayError::Invalid);
            }
            if question + 1 == draft.answers.len() {
                let response = action_response(
                    &p.card.context,
                    InteractionAction::Answers {
                        answers: draft.answers.clone(),
                    },
                    &route.identity.adapter_instance_id,
                )?;
                pending
                    .remove(&request)
                    .ok_or(GatewayError::StaleInteraction)?
                    .response
                    .send(response)
                    .map_err(|_| GatewayError::StaleInteraction)?;
                true
            } else {
                draft.index += 1;
                draft.offset = 0;
                draft.revision = draft.revision.checked_add(1).ok_or(GatewayError::Invalid)?;
                false
            }
        };
        if complete {
            Ok(MenuCard {
                title: "回答已提交".into(),
                text: "Agent 将使用你的回答继续当前任务。可发送 /状态 查看进度。".into(),
                buttons: Vec::new(),
                question_request: None,
            })
        } else {
            self.question_menu(route, request).await
        }
    }

    pub(crate) async fn question_text(
        &self,
        route: &ChannelRoute,
        request: Uuid,
        text: &str,
    ) -> GatewayResult<MenuCard> {
        if text.trim().is_empty() || text.len() > 4096 {
            return Err(GatewayError::Invalid);
        }
        let (_, card, current) = self.owned_question(route, request).await?;
        let InteractionContext::Questions { requests } = &card.context else {
            return Err(GatewayError::Invalid);
        };
        let question = &requests[current.index];
        let normalized = text.trim().replace('，', ",");
        let mut selected = Vec::new();
        let mut all_options = !question.options.is_empty();
        for part in normalized.split(',').map(str::trim) {
            let option = question
                .options
                .iter()
                .find(|o| o.label == part)
                .or_else(|| {
                    part.parse::<usize>()
                        .ok()
                        .and_then(|i| i.checked_sub(1))
                        .and_then(|i| question.options.get(i))
                });
            match option {
                Some(o) => selected.push(o.label.clone()),
                None => all_options = false,
            }
        }
        if all_options
            && ((!question.multi_select && selected.len() != 1)
                || selected.iter().collect::<HashSet<_>>().len() != selected.len())
        {
            return Err(GatewayError::Invalid);
        }
        {
            let mut pending = self.pending.lock();
            let p = pending
                .get_mut(&request)
                .ok_or(GatewayError::StaleInteraction)?;
            let draft = p.draft.as_mut().ok_or(GatewayError::Invalid)?;
            if draft.revision != current.revision
                || draft.index != current.index
                || p.card.expires_at <= self.identity.now()
                || p.response.is_closed()
            {
                return Err(GatewayError::StaleInteraction);
            }
            draft.answers[current.index] = QuestionAnswer {
                id: question.id.clone(),
                selected: if all_options { selected } else { Vec::new() },
                text: if all_options {
                    None
                } else {
                    Some(text.trim().into())
                },
            };
            draft.revision = draft.revision.checked_add(1).ok_or(GatewayError::Invalid)?;
        }
        self.question_next(route, request, current.revision + 1, current.index)
            .await
    }
}
