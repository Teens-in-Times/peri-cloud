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
        let (verb, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        let section = match verb {
            "/设置" => "home",
            "/记忆" | "/记住" => "memory",
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
        if matches!(section, "memory" | "instructions") {
            let mut data = self.journal().user_data(principal).await?;
            if rest == "清空" {
                let action = self
                    .store
                    .save_menu_action(
                        &message.route,
                        principal,
                        MenuRecord {
                            action: MenuAction::ClearPersonal {
                                field: section.into(),
                                revision: data.revision,
                            },
                            expected_session: selected,
                            expires_at: self.identity.now() + 240,
                        },
                    )
                    .await?;
                return Ok(Some(DeliveryBody::Menu{card:MenuCard{title:"确认清空个人数据？".into(),text:format!("仅清空你的{}，不影响任务记录和其他账号。网页或另一条消息已修改数据时，本次确认将失效。",if section=="memory" {"记忆"}else{"偏好"}),buttons:vec![MenuButton{label:"确认清空".into(),action_id:action}],question_request:None}}));
            }
            let append = verb == "/记住";
            let content = if append {
                Some(rest)
            } else {
                rest.strip_prefix("设置 ")
                    .or_else(|| rest.strip_prefix("替换 "))
            };
            if let Some(content) = content {
                let content = content.trim();
                if content.is_empty() {
                    return Err(GatewayError::Invalid);
                }
                let field = if section == "memory" {
                    &mut data.memory
                } else {
                    &mut data.instructions
                };
                if append && !field.is_empty() {
                    field.push('\n');
                } else if !append {
                    field.clear();
                }
                field.push_str(content);
                self.journal()
                    .save_user_data(principal, data, self.limits().max_user_data_bytes)
                    .await?;
                return Ok(Some(
                    "已保存，下一轮对话生效。发送 /记忆 或 /偏好 可查看。".into(),
                ));
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
    pub(super) async fn clear_personal(
        &self,
        principal: Uuid,
        field: &str,
        revision: u64,
    ) -> GatewayResult<String> {
        let mut data = self.journal().user_data(principal).await?;
        if data.revision != revision {
            return Err(StateError::InvocationConflict.into());
        }
        match field {
            "memory" => data.memory.clear(),
            "instructions" => data.instructions.clear(),
            _ => return Err(GatewayError::Invalid),
        }
        self.journal()
            .save_user_data(principal, data, self.limits().max_user_data_bytes)
            .await?;
        Ok("已清空所选个人数据，下一轮对话生效。".into())
    }
    pub(super) async fn personal_menu(
        &self,
        message: &InboundMessage,
        principal: Uuid,
        selected: Option<Uuid>,
        section: &str,
        page: usize,
    ) -> GatewayResult<MenuCard> {
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
            "home"=>("个人设置".to_owned(),"QQ 内可查看用量、管理记忆与偏好、切换已配置的个人模型。设置属于你的账号，在所有设备间共用，下一轮对话生效。\n\n/记住 内容：追加记忆\n/记忆 设置 内容：替换记忆\n/偏好 设置 内容：设置回答与工作偏好\n/记忆 清空 或 /偏好 清空：确认后清除\n/模型 模型名：使用已绑定的个人密钥切换模型".to_owned()),
            "memory"|"instructions"=>{
                let data=self.journal().user_data(principal).await?;
                let value=if section=="memory" {data.memory}else{data.instructions};
                let count=value.chars().count().div_ceil(1200).max(1);
                if page==0 || page>count {return Err(GatewayError::Invalid)}
                let body:String=value.chars().skip((page-1)*1200).take(1200).collect();
                if page>1 {choices.push(("上一页".into(),MenuAction::Personal{section:section.into(),page:page-1}));}
                if page<count {choices.push(("下一页".into(),MenuAction::Personal{section:section.into(),page:page+1}));}
                (format!("个人{} · {page}/{count}",if section=="memory" {"记忆"}else{"偏好"}),if body.is_empty(){"尚未设置。发送 /记住 内容 或 /偏好 设置 内容。".into()}else{body})
            },
            "usage"=>{let usage=self.journal().user_usage(principal).await?;let limits=self.limits();("今日用量（UTC）".into(),format!("已提交任务：{} / {}\n模型请求：{} / {}\n最多同时执行：{} 个任务\n偏好与记忆上限：{} 字节",usage.turns,limits.daily_turns,usage.model_requests,limits.daily_model_requests,limits.max_active_per_user,limits.max_user_data_bytes))},
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
