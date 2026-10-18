//! QQ account controls share the portal's stores and revision checks.
use super::*;
use crate::gateway::menu_store::{MenuAction, MenuRecord};
use crate::gateway::{MenuButton, MenuCard};

impl Gateway {
    pub(super) async fn personal_command(
        &self,
        message: &InboundMessage,
        text: &str,
        principal: Uuid,
        selected: Option<Uuid>,
    ) -> GatewayResult<Option<DeliveryBody>> {
        if let Some(reply) = self
            .memory_command(message, text, principal, selected)
            .await?
        {
            return Ok(Some(reply));
        }
        let (verb, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        let section = match verb {
            "/设置" => "home",
            "/偏好" => "instructions",
            "/用量" => "usage",
            "/模型" => "model",
            _ => return Ok(None),
        };
        let rest = rest.trim();
        if verb == "/模型" {
            if let Some(aggregate) = self
                .connector
                .account_models()
                .and_then(|m| m.aggregation())
            {
                if rest.is_empty() {
                    return Ok(Some(DeliveryBody::Menu {
                        card: self
                            .aggregate_model_menu(message, principal, selected, 1)
                            .await?,
                    }));
                }
                let current = aggregate
                    .selection(
                        &crate::aggregation::ControlActor::Channel(&message.route.identity),
                        selected,
                    )
                    .await
                    .map_err(|_| GatewayError::Connection)?;
                return Ok(Some(
                    self.choose_aggregate_model(
                        message,
                        selected,
                        if rest == "默认" {
                            None
                        } else {
                            Some(rest.into())
                        },
                        current.revision,
                    )
                    .await?,
                ));
            }
        }

        if verb == "/模型" && !rest.is_empty() {
            if rest.chars().any(char::is_whitespace) {
                return Err(GatewayError::Invalid);
            }
            let Some(models) = self.connector.account_models() else {
                return Ok(Some("当前服务没有开启个人模型设置。".into()));
            };
            let Some(choice) = models
                .choice(principal)
                .await
                .map_err(|_| GatewayError::Connection)?
            else {
                return Ok(Some("当前使用服务默认模型。首次绑定个人模型密钥需在账号设置完成一次，之后可在 QQ 发送 /模型 模型名 切换；请勿在聊天里发送密钥。".into()));
            };
            models
                .save(
                    principal,
                    crate::models::ModelUpdate {
                        provider: choice.provider,
                        model: rest.into(),
                        api_key: None,
                        ..Default::default()
                    },
                )
                .await
                .map_err(|_| GatewayError::Invalid)?;
            return Ok(Some(
                format!(
                    "个人模型已改为 {}，下一轮对话生效。当前任务继续使用已选模型。",
                    rest
                )
                .into(),
            ));
        }
        if section == "instructions" {
            if !self.private_memory_route(&message.route) {
                return Ok(Some("个人偏好请在私聊或已登录网页管理。".into()));
            }
            let mut data = self.journal().preferences(principal).await?;
            if rest == "清空" {
                let id = self
                    .store
                    .save_menu_action(
                        &message.route,
                        principal,
                        MenuRecord {
                            action: MenuAction::ClearPreferences {
                                revision: data.revision,
                            },
                            expected_session: selected,
                            expires_at: self.identity.now() + 240,
                        },
                    )
                    .await?;
                return Ok(Some(DeliveryBody::Menu {
                    card: MenuCard {
                        title: "确认清空回复偏好？".into(),
                        text: "只清空回复偏好。资料版本已变化时旧确认失效。".into(),
                        buttons: vec![MenuButton {
                            label: "确认清空".into(),
                            action_id: id,
                        }],
                        question_request: None,
                    },
                }));
            }
            if let Some(content) = rest.strip_prefix("设置 ") {
                data.instructions = content.trim().into();
                self.journal().save_preferences(principal, data).await?;
                return Ok(Some("回复偏好已保存，下一轮生效。".into()));
            }
        }
        let page = if rest.is_empty() {
            1
        } else {
            rest.parse::<usize>().map_err(|_| GatewayError::Invalid)?
        };
        Ok(Some(DeliveryBody::Menu {
            card: self
                .personal_menu(message, principal, selected, section, page)
                .await?,
        }))
    }
    pub(super) async fn clear_preferences(
        &self,
        principal: Uuid,
        revision: u64,
    ) -> GatewayResult<String> {
        self.journal()
            .save_preferences(
                principal,
                crate::state::UserPreferences {
                    revision,
                    instructions: String::new(),
                },
            )
            .await?;
        Ok("回复偏好已清空，下一轮生效。".into())
    }
    pub(super) async fn personal_menu(
        &self,
        message: &InboundMessage,
        principal: Uuid,
        selected: Option<Uuid>,
        section: &str,
        page: usize,
    ) -> GatewayResult<MenuCard> {
        if section == "memory" {
            return self.memory_menu(message, principal, selected, page).await;
        }
        if section == "instructions" && !self.private_memory_route(&message.route) {
            return Err(IdentityError::Forbidden.into());
        }
        if section == "model"
            && self
                .connector
                .account_models()
                .and_then(|m| m.aggregation())
                .is_some()
        {
            return self
                .aggregate_model_menu(message, principal, selected, page)
                .await;
        }
        let mut choices = Vec::new();
        let (title,text)=match section {
            "home"=>("个人设置".to_owned(),"/记忆：逐条查看、搜索、更正和忘记\n/记住 内容：确认后添加个人事实\n/偏好 设置 内容：设置回答偏好\n/偏好 清空：确认后清除偏好\n/模型 模型名：切换已配置的模型\n\n个人资料仅在私聊与已登录网页管理。".to_owned()),
            "instructions"=>{
                let data=self.journal().preferences(principal).await?;
                let value=data.instructions;
                let count=value.chars().count().div_ceil(1200).max(1);
                if page==0 || page>count {return Err(GatewayError::Invalid)}
                let body:String=value.chars().skip((page-1)*1200).take(1200).collect();
                if page>1 {choices.push(("上一页".into(),MenuAction::Personal{section:section.into(),page:page-1}));}
                if page<count {choices.push(("下一页".into(),MenuAction::Personal{section:section.into(),page:page+1}));}
                (format!("回复偏好 · {page}/{count}"),if body.is_empty(){"尚未设置。发送 /偏好 设置 内容。".into()}else{body})
            },
            "usage"=>{let usage=self.journal().user_usage(principal).await?;let limits=self.limits();("今日用量（UTC）".into(),format!("已提交任务：{} / {}\n模型请求：{} / {}\n最多同时执行：{} 个任务\n记忆容量上限：{} 字节",usage.turns,limits.daily_turns,usage.model_requests,limits.daily_model_requests,limits.max_active_per_user,limits.max_memory_bytes))},
            "model"=>{
                let text=if let Some(models)=self.connector.account_models() {
                    let choice=models.choice(principal).await.map_err(|_|GatewayError::Connection)?;
                    format!("当前：{}\n{}\n\n已绑定个人密钥后，发送 /模型 模型名 即可切换，下一轮生效。",choice.as_ref().map(|c|format!("{} · {}",c.model,c.provider)).unwrap_or_else(||format!("{} · 服务默认",models.default_model())),models.providers().iter().map(|p|format!("供应商：{}（{}）",p.name,p.id)).collect::<Vec<_>>().join("\n"))
                } else {"使用服务默认模型；当前未开启个人模型设置。".into()};
                ("模型设置".into(),text)
            },
            _=>return Err(GatewayError::Invalid)
        };
        if section == "home" {
            for (label, name) in [
                ("记忆", "memory"),
                ("偏好", "instructions"),
                ("用量", "usage"),
                ("模型", "model"),
            ] {
                choices.push((
                    label.into(),
                    MenuAction::Personal {
                        section: name.into(),
                        page: 1,
                    },
                ));
            }
        } else {
            choices.push((
                "返回设置".into(),
                MenuAction::Personal {
                    section: "home".into(),
                    page: 1,
                },
            ));
        }
        let mut buttons = Vec::new();
        for (label, action) in choices {
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
        Ok(MenuCard {
            title,
            text,
            buttons,
            question_request: None,
        })
    }
}
