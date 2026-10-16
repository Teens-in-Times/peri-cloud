use super::store::AccessInput;
use super::{AccessChange, AccessList, AccessMode, AccessRecord, ConversationScope};
use crate::gateway::menu_store::MenuAction;
use crate::gateway::{
    ChannelRoute, DeliveryBody, Gateway, GatewayError, GatewayResult, InboundMessage,
};
use crate::identity::IdentityError;
use uuid::Uuid;

fn action(adapter: Option<&str>, section: &str, page: usize) -> MenuAction {
    MenuAction::Access {
        adapter: adapter.map(str::to_owned),
        section: section.into(),
        page,
    }
}
fn list(section: &str) -> GatewayResult<AccessList> {
    match section {
        "allowed_users" => Ok(AccessList::AllowedUsers),
        "allowed_groups" => Ok(AccessList::AllowedGroups),
        "blocked_users" => Ok(AccessList::BlockedUsers),
        "blocked_groups" => Ok(AccessList::BlockedGroups),
        _ => Err(GatewayError::Invalid),
    }
}
fn label(list: AccessList) -> &'static str {
    match list {
        AccessList::AllowedUsers => "允许的用户",
        AccessList::AllowedGroups => "允许的群",
        AccessList::BlockedUsers => "屏蔽的用户",
        AccessList::BlockedGroups => "屏蔽的群",
    }
}

impl Gateway {
    pub async fn route_allowed(&self, route: &ChannelRoute) -> GatewayResult<bool> {
        route.key()?;
        let adapter = self
            .adapters
            .get(&route.identity.adapter_instance_id)
            .ok_or(GatewayError::Invalid)?;
        let record = self.store.access_policy(adapter.instance_id()).await?;
        Ok(record
            .policy
            .allows(route, &adapter.conversation_scope(route)))
    }
    async fn require_access_admin(&self, message: &InboundMessage) -> GatewayResult<Uuid> {
        let adapter = self
            .adapters
            .get(&message.route.identity.adapter_instance_id)
            .ok_or(GatewayError::Invalid)?;
        if adapter.conversation_scope(&message.route) != ConversationScope::Private
            || !self
                .identity
                .channel_is_admin(&message.route.identity)
                .await?
        {
            return Err(IdentityError::Forbidden.into());
        }
        Ok(self
            .identity
            .resolve_channel(&message.route.identity)
            .await?)
    }
    pub(in crate::gateway) async fn access_command(
        &self,
        message: &InboundMessage,
        principal: Uuid,
    ) -> GatewayResult<Option<DeliveryBody>> {
        let text = message.text.trim();
        if text == "/取消设置" {
            self.store.clear_access_input(&message.route).await?;
            return Ok(Some(
                "已退出名单输入。发送 /网关 返回管理菜单；当前任务不会因此取消。".into(),
            ));
        }
        if text == "/网关" || text == "/访问控制" {
            match self.require_access_admin(message).await {
                Ok(_) => (),
                Err(GatewayError::Identity(IdentityError::Forbidden)) => {
                    return Ok(Some(
                        "访问控制仅供已关联的管理员在私聊中管理；群聊不展示名单。".into(),
                    ))
                }
                Err(error) => return Err(error),
            }
            self.store.clear_access_input(&message.route).await?;
            return self
                .access_menu(message, principal, None, "home", 1)
                .await
                .map(Some);
        }
        let Some(input) = self.store.access_input(&message.route, principal).await? else {
            return Ok(None);
        };
        self.require_access_admin(message).await?;
        if input.expires_at <= self.identity.now() {
            self.store.clear_access_input(&message.route).await?;
            return Ok(Some(
                "名单输入已过期；发送 /网关 重新选择。没有调用 Agent。".into(),
            ));
        }
        if text.starts_with('/') {
            self.store.clear_access_input(&message.route).await?;
            return Ok(
                (text == "/取消设置").then(|| "已取消名单输入。发送 /网关 返回管理菜单。".into())
            );
        }
        let adapter = self
            .adapters
            .get(&input.adapter)
            .ok_or(GatewayError::Invalid)?;
        if !adapter.valid_access_id(input.list, text) {
            return Ok(Some(
                "标识不正确，请只发送一个用户号或群号；发送 /取消设置 退出。没有调用 Agent。"
                    .into(),
            ));
        }
        let body = self
            .access_change(
                message,
                principal,
                &input.adapter,
                input.revision,
                &AccessChange::Add {
                    list: input.list,
                    id: text.into(),
                },
            )
            .await?;
        self.store.clear_access_input(&message.route).await?;
        Ok(Some(body))
    }
    pub(in crate::gateway) async fn access_prompt(
        &self,
        message: &InboundMessage,
        principal: Uuid,
        adapter: &str,
        revision: u64,
        list: AccessList,
    ) -> GatewayResult<DeliveryBody> {
        if self.require_access_admin(message).await? != principal {
            return Err(IdentityError::Forbidden.into());
        }
        let current = self.store.access_policy(adapter).await?;
        if current.revision != revision {
            return Err(crate::state::StateError::InvocationConflict.into());
        }
        self.store
            .set_access_input(
                &message.route,
                principal,
                &AccessInput {
                    adapter: adapter.into(),
                    revision,
                    list,
                    expires_at: self.identity.now() + 240,
                },
            )
            .await?;
        Ok(format!("添加{} · 入口 {adapter}\n请直接发送一个完整的用户标识或群标识；个人 QQ 请填 QQ 号或群号。4 分钟内有效。\n这次输入用于名单设置，不会交给 Agent。发送 /取消设置 退出。",label(list)).into())
    }
    pub(in crate::gateway) async fn access_change(
        &self,
        message: &InboundMessage,
        principal: Uuid,
        adapter: &str,
        revision: u64,
        change: &AccessChange,
    ) -> GatewayResult<DeliveryBody> {
        let _write = self.access_gate.write().await;
        if self.require_access_admin(message).await? != principal {
            return Err(IdentityError::Forbidden.into());
        }
        let target = self.adapters.get(adapter).ok_or(GatewayError::Invalid)?;
        let current = self.store.access_policy(adapter).await?;
        if current.revision != revision {
            return Err(crate::state::StateError::InvocationConflict.into());
        }
        if let AccessChange::Add { list, id } | AccessChange::Remove { list, id } = change {
            if !target.valid_access_id(*list, id) {
                return Err(GatewayError::Invalid);
            }
        }
        let next = current.policy.changed(change)?;
        if adapter == message.route.identity.adapter_instance_id
            && !next.allows(&message.route, &target.conversation_scope(&message.route))
        {
            return Ok("这会关闭当前管理入口，修改未保存。请保留自己的私聊权限，或从另一处已关联的管理员入口修改。".into());
        }
        self.store
            .save_access(principal, adapter, revision, &next, self.identity.now())
            .await?;
        self.access_menu(message, principal, Some(adapter), "home", 1)
            .await
    }
    pub(in crate::gateway) async fn access_menu(
        &self,
        message: &InboundMessage,
        principal: Uuid,
        adapter: Option<&str>,
        section: &str,
        page: usize,
    ) -> GatewayResult<DeliveryBody> {
        if self.require_access_admin(message).await? != principal {
            return Err(IdentityError::Forbidden.into());
        }
        let mut choices = Vec::new();
        let (title, text) = if let Some(adapter) = adapter {
            if !self.adapters.contains_key(adapter) {
                return Err(GatewayError::Invalid);
            }
            let AccessRecord { revision, policy } = self.store.access_policy(adapter).await?;
            if section == "home" {
                choices.extend([
                    (
                        format!("允许的用户（{}）", policy.allowed_users.len()),
                        action(Some(adapter), "allowed_users", 1),
                    ),
                    (
                        format!("允许的群（{}）", policy.allowed_groups.len()),
                        action(Some(adapter), "allowed_groups", 1),
                    ),
                    (
                        format!("屏蔽的用户（{}）", policy.blocked_users.len()),
                        action(Some(adapter), "blocked_users", 1),
                    ),
                    (
                        format!("屏蔽的群（{}）", policy.blocked_groups.len()),
                        action(Some(adapter), "blocked_groups", 1),
                    ),
                    ("切换名单模式".into(), action(Some(adapter), "mode", 1)),
                    ("返回入口列表".into(), action(None, "home", 1)),
                ]);
                let mode = if policy.mode == AccessMode::Allowlist {
                    "白名单：只响应允许的用户；群聊还须允许该群。"
                } else {
                    "黑名单：响应未屏蔽的用户和群；仍须账号关联与工具权限。"
                };
                (format!("入口访问控制 · {adapter}"),format!("{mode}\n黑名单优先；新设置从之后的入站与发件生效，已发出的消息不能撤回，运行中任务不会自动取消。\n版本：{revision}\n本菜单仅管理员私聊可用。"))
            } else if section == "mode" {
                choices.push((
                    "使用白名单模式".into(),
                    action(Some(adapter), "confirm_allowlist", 1),
                ));
                choices.push((
                    "使用黑名单模式".into(),
                    action(Some(adapter), "confirm_blocklist", 1),
                ));
                choices.push(("返回".into(), action(Some(adapter), "home", 1)));
                ("选择名单模式".into(),"白名单默认拒绝；黑名单默认接受未屏蔽的聊天来源。两种模式都不会跳过 Peri 账号关联、会话归属与工具审批。".into())
            } else if matches!(section, "confirm_allowlist" | "confirm_blocklist") {
                let mode = if section == "confirm_allowlist" {
                    AccessMode::Allowlist
                } else {
                    AccessMode::Blocklist
                };
                choices.push((
                    "确认切换".into(),
                    MenuAction::AccessChange {
                        adapter: adapter.into(),
                        revision,
                        change: AccessChange::Mode { mode },
                    },
                ));
                choices.push(("保持原模式".into(), action(Some(adapter), "home", 1)));
                (
                    "确认切换访问模式".into(),
                    if mode == AccessMode::Blocklist {
                        "切换后，名单外的群和用户也可向机器人发送命令；黑名单继续生效。未关联用户仍不能执行任务。".into()
                    } else {
                        "切换后，仅白名单用户可私聊；群聊必须同时允许该用户和该群。黑名单继续生效。"
                            .into()
                    },
                )
            } else {
                let kind = list(section)?;
                let entries = policy.list(kind);
                let pages = entries.len().div_ceil(6).max(1);
                if page == 0 || page > pages {
                    return Err(GatewayError::Invalid);
                }
                let mut lines = vec![format!("共 {} 项 · 第 {page}/{pages} 页", entries.len())];
                for id in entries.iter().skip((page - 1) * 6).take(6) {
                    lines.push(id.clone());
                    choices.push((
                        format!("移除 {id}"),
                        MenuAction::AccessChange {
                            adapter: adapter.into(),
                            revision,
                            change: AccessChange::Remove {
                                list: kind,
                                id: id.clone(),
                            },
                        },
                    ));
                }
                choices.push((
                    "添加一项".into(),
                    MenuAction::AccessInput {
                        adapter: adapter.into(),
                        revision,
                        list: kind,
                    },
                ));
                if page > 1 {
                    choices.push(("上一页".into(), action(Some(adapter), section, page - 1)));
                }
                if page < pages {
                    choices.push(("下一页".into(), action(Some(adapter), section, page + 1)));
                }
                choices.push(("返回访问控制".into(), action(Some(adapter), "home", 1)));
                (format!("{} · {adapter}", label(kind)), lines.join("\n"))
            }
        } else {
            let mut ids: Vec<_> = self.adapters.keys().collect();
            ids.sort();
            let pages = ids.len().div_ceil(8).max(1);
            if page == 0 || page > pages {
                return Err(GatewayError::Invalid);
            }
            for id in ids.into_iter().skip((page - 1) * 8).take(8) {
                choices.push((format!("管理 {id}"), action(Some(id), "home", 1)));
            }
            if page > 1 {
                choices.push(("上一页".into(), action(None, "home", page - 1)));
            }
            if page < pages {
                choices.push(("下一页".into(), action(None, "home", page + 1)));
            }
            ("网关与访问控制".into(),format!("选择要管理的入口。每个入口独立设置，名单不按昵称或号码跨平台合并。\n第 {page}/{pages} 页"))
        };
        self.build_menu(message, principal, title, text, choices)
            .await
    }
}
