//! QQ memory controls are deterministic, private and share the item service.
use super::*;
use crate::gateway::menu_store::{MenuAction, MenuRecord};
use crate::gateway::{access, MenuButton, MenuCard};
use crate::memory::{
    MemoryContent, MemoryItem, MemoryKind, MemoryScope, MemorySource, MemoryStore,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(crate) enum MemoryChange {
    Add {
        #[serde(default)]
        scope: MemoryScope,
        id: Uuid,
        generation: u64,
        value: MemoryContent,
        source: MemorySource,
    },
    Update {
        #[serde(default)]
        scope: MemoryScope,
        id: Uuid,
        revision: u64,
        value: MemoryContent,
        source: MemorySource,
    },
    Forget {
        #[serde(default)]
        scope: MemoryScope,
        id: Uuid,
        revision: u64,
        source: MemorySource,
    },
}

impl Gateway {
    pub(super) fn private_memory_route(&self, route: &ChannelRoute) -> bool {
        self.adapters
            .get(&route.identity.adapter_instance_id)
            .is_some_and(|adapter| {
                adapter.conversation_scope(route) == access::ConversationScope::Private
            })
    }
    fn memory_store(&self, principal: Uuid) -> MemoryStore {
        MemoryStore::new(self.journal(), principal, self.limits().max_memory_bytes).for_management()
    }
    pub(super) async fn memory_command(
        &self,
        message: &InboundMessage,
        text: &str,
        principal: Uuid,
        selected: Option<Uuid>,
    ) -> GatewayResult<Option<DeliveryBody>> {
        let (verb, rest) = text
            .trim()
            .split_once(char::is_whitespace)
            .unwrap_or((text.trim(), ""));
        if !matches!(verb, "/记忆" | "/记住") {
            return Ok(None);
        }
        if !self.private_memory_route(&message.route) {
            return Ok(Some(
                "个人记忆只在私聊和已登录网页中使用，请到私聊管理。".into(),
            ));
        }
        let (scope, rest) = parse_scope(rest.trim())?;
        let store = self.memory_store(principal).scoped(scope.clone());
        let source = MemorySource::qq(selected, message.key()?);
        if verb == "/记住" {
            let page = store.list("", 0, 1).await?;
            let value = MemoryContent {
                content: rest.into(),
                kind: MemoryKind::Fact,
                pinned: false,
                valid_until: None,
            };
            return self
                .memory_confirmation(
                    message,
                    principal,
                    selected,
                    MemoryChange::Add {
                        scope: scope.clone(),
                        id: Uuid::new_v4(),
                        generation: page.generation,
                        value,
                        source,
                    },
                    rest,
                )
                .await
                .map(Some);
        }
        if let Some(query) = rest.strip_prefix("搜索 ") {
            let page = store.list(query, 0, 5).await?;
            let text = page
                .items
                .iter()
                .map(describe)
                .collect::<Vec<_>>()
                .join("\n\n");
            return Ok(Some(DeliveryBody::Menu {
                card: MenuCard {
                    title: "搜索个人记忆".into(),
                    text: format!(
                        "{}{}",
                        if text.is_empty() {
                            "没有找到有效记忆。"
                        } else {
                            &text
                        },
                        if page.has_more {
                            "\n还有更多结果，可缩短关键词或在网页查看。"
                        } else {
                            ""
                        }
                    ),
                    buttons: vec![],
                    question_request: None,
                },
            }));
        }
        let (action, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        if matches!(
            action,
            "查看" | "修改" | "忘记" | "置顶" | "取消置顶" | "期限"
        ) {
            let (id, content) = args
                .trim()
                .split_once(char::is_whitespace)
                .unwrap_or((args.trim(), ""));
            let item = store
                .inspect(Uuid::parse_str(id).map_err(|_| GatewayError::Invalid)?)
                .await?;
            if action == "查看" {
                return Ok(Some(DeliveryBody::Menu {
                    card: MenuCard {
                        title: "个人记忆详情".into(),
                        text: describe(&item),
                        buttons: vec![],
                        question_request: None,
                    },
                }));
            }
            let change = if action == "忘记" {
                MemoryChange::Forget {
                    scope: scope.clone(),
                    id: item.id,
                    revision: item.revision,
                    source,
                }
            } else {
                let mut value = item.value.clone();
                match action {
                    "修改" => value.content = content.trim().into(),
                    "置顶" => value.pinned = true,
                    "取消置顶" => value.pinned = false,
                    "期限" => {
                        value.valid_until = if content.trim() == "永久" {
                            None
                        } else {
                            Some(
                                chrono::DateTime::parse_from_rfc3339(content.trim())
                                    .map_err(|_| GatewayError::Invalid)?
                                    .timestamp(),
                            )
                        }
                    }
                    _ => unreachable!(),
                }
                MemoryChange::Update {
                    scope: scope.clone(),
                    id: item.id,
                    revision: item.revision,
                    value,
                    source,
                }
            };
            let details = match &change {
                MemoryChange::Update { value, .. } => format!(
                    "原内容：\n{}\n\n修改为：\n{}\n置顶：{}\n有效至：{:?}",
                    item.value.content, value.content, value.pinned, value.valid_until
                ),
                _ => format!(
                    "{}\n\n遗忘会移除正文与索引，聊天历史和备份仍独立保留。",
                    item.value.content
                ),
            };
            return self
                .memory_confirmation(message, principal, selected, change, &details)
                .await
                .map(Some);
        }
        let page = if rest.is_empty() {
            1
        } else {
            rest.parse::<usize>().map_err(|_| GatewayError::Invalid)?
        };
        Ok(Some(DeliveryBody::Menu {
            card: self
                .memory_scope_menu(message, principal, selected, page, scope)
                .await?,
        }))
    }
    pub(super) async fn memory_menu(
        &self,
        message: &InboundMessage,
        principal: Uuid,
        selected: Option<Uuid>,
        page: usize,
    ) -> GatewayResult<MenuCard> {
        self.memory_scope_menu(message, principal, selected, page, MemoryScope::User)
            .await
    }
    pub(super) async fn memory_scope_menu(
        &self,
        message: &InboundMessage,
        principal: Uuid,
        selected: Option<Uuid>,
        page: usize,
        scope: MemoryScope,
    ) -> GatewayResult<MenuCard> {
        if !self.private_memory_route(&message.route) {
            return Err(IdentityError::Forbidden.into());
        }
        if page == 0 {
            return Err(GatewayError::Invalid);
        }
        let entries = self
            .memory_store(principal)
            .scoped(scope.clone())
            .inventory("", (page - 1).saturating_mul(3), 3)
            .await?;
        let mut text = format!(
            "{} 条 · {} / {} 字节\n范围：{}（私聊）；资料是事实，不授予权限。\n\n",
            entries.total,
            entries.used_bytes,
            self.limits().max_memory_bytes,
            scope.key()
        );
        text.push_str(
            &entries
                .items
                .iter()
                .map(describe)
                .collect::<Vec<_>>()
                .join("\n\n"),
        );
        text.push_str("\n\n可在命令后加范围：个人、设备池、设备 UUID。例如 /记住 设备池 小蓝是测试机；/记忆 设备 UUID 搜索 Python。设备 ID 见 /设备。\n/记住 内容：确认后添加\n/记忆 搜索 关键词\n/记忆 查看 UUID\n/记忆 修改 UUID 内容\n/记忆 置顶 UUID（或 取消置顶）\n/记忆 期限 UUID RFC3339时间（或 永久）\n/记忆 忘记 UUID");
        let mut buttons = Vec::new();
        for (label, next) in [("上一页", page.saturating_sub(1)), ("下一页", page + 1)] {
            if next == 0 || (next > page && !entries.has_more) {
                continue;
            }
            let id = self
                .store
                .save_menu_action(
                    &message.route,
                    principal,
                    MenuRecord {
                        action: MenuAction::MemoryPage {
                            scope: scope.clone(),
                            page: next,
                        },
                        expected_session: selected,
                        expires_at: self.identity.now() + 240,
                    },
                )
                .await?;
            buttons.push(MenuButton {
                label: label.into(),
                action_id: id,
            });
        }
        Ok(MenuCard {
            title: format!("我的记忆 · 第 {page} 页"),
            text,
            buttons,
            question_request: None,
        })
    }
    async fn memory_confirmation(
        &self,
        message: &InboundMessage,
        principal: Uuid,
        selected: Option<Uuid>,
        change: MemoryChange,
        details: &str,
    ) -> GatewayResult<DeliveryBody> {
        if details.trim().is_empty() || details.len() > 24000 {
            return Err(GatewayError::Invalid);
        }
        let scope = match &change {
            MemoryChange::Add { scope, .. }
            | MemoryChange::Update { scope, .. }
            | MemoryChange::Forget { scope, .. } => scope.key(),
        };
        let id = self
            .store
            .save_menu_action(
                &message.route,
                principal,
                MenuRecord {
                    action: MenuAction::MemoryChange { change },
                    expected_session: selected,
                    expires_at: self.identity.now() + 240,
                },
            )
            .await?;
        Ok(DeliveryBody::Menu {card:MenuCard {title:"确认修改个人记忆？".into(),text:format!("范围：{scope}，属于你的账号，仅私聊可用。确认后下一轮生效；并发修改会使旧确认失效。\n\n{details}"),buttons:vec![MenuButton {label:"确认保存".into(),action_id:id}],question_request:None}})
    }
    pub(super) async fn apply_memory_change(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
        change: MemoryChange,
    ) -> GatewayResult<String> {
        if !self.private_memory_route(route) {
            return Err(IdentityError::Forbidden.into());
        }
        let store = self.memory_store(principal);
        match change {
            MemoryChange::Add {
                scope,
                id,
                generation,
                value,
                source,
            } => {
                store
                    .scoped(scope)
                    .add(id, generation, value, source)
                    .await?;
            }
            MemoryChange::Update {
                scope,
                id,
                revision,
                value,
                source,
            } => {
                store
                    .scoped(scope)
                    .update(id, revision, value, source)
                    .await?;
            }
            MemoryChange::Forget {
                scope,
                id,
                revision,
                source,
            } => {
                store.scoped(scope).forget(id, revision, source).await?;
            }
        }
        Ok("个人记忆已更新，下一轮生效。可通过 /记忆 查看、更正或忘记。".into())
    }
}
fn describe(item: &MemoryItem) -> String {
    format!(
        "{} · v{} · {}{}\n{}\n来源：{}；更新：{}；有效至：{}",
        item.id,
        item.revision,
        if item.value.pinned {
            "置顶"
        } else {
            "按需检索"
        },
        if item
            .value
            .valid_until
            .is_some_and(|at| at <= crate::memory::now())
        {
            " · 已过期"
        } else {
            ""
        },
        item.value.content,
        item.source.kind,
        if item.updated_at == 0 {
            "旧版导入，原时间未知".into()
        } else {
            item.updated_at.to_string()
        },
        item.value
            .valid_until
            .map(|v| v.to_string())
            .unwrap_or_else(|| "永久".into())
    )
}

fn parse_scope(text: &str) -> GatewayResult<(MemoryScope, &str)> {
    if text == "个人" {
        return Ok((MemoryScope::User, ""));
    }
    if let Some(rest) = text.strip_prefix("个人 ") {
        return Ok((MemoryScope::User, rest.trim()));
    }
    if text == "设备池" {
        return Ok((MemoryScope::Fleet, ""));
    }
    if let Some(rest) = text.strip_prefix("设备池 ") {
        return Ok((MemoryScope::Fleet, rest.trim()));
    }
    if let Some(rest) = text.strip_prefix("设备 ") {
        let (id, rest) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let device_id = Uuid::parse_str(id).map_err(|_| GatewayError::Invalid)?;
        return Ok((MemoryScope::DeviceEnv { device_id }, rest.trim()));
    }
    Ok((MemoryScope::User, text))
}
