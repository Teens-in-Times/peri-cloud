use peri_acp_types::device_executor::DirectoryQuery;
use peri_acp_types::permission::PermissionMode;
use std::sync::Arc;
use uuid::Uuid;

use super::{permission_label, Gateway};
use crate::gateway::menu_store::{MenuAction, MenuRecord};
use crate::gateway::{
    ChannelRoute, DeliveryBody, GatewayError, GatewayResult, InboundMessage, MenuButton, MenuCard,
};
use crate::identity::{DeviceRecord, IdentityError};

impl Gateway {
    /// ACK means queued, not completed. Deployment owns directory I/O and
    /// selection after ACK; no QQ interaction waiter can cancel that work.
    pub async fn receive_menu(
        self: &Arc<Self>,
        route: ChannelRoute,
        event: String,
        action: Uuid,
    ) -> GatewayResult<()> {
        if !self.route_allowed(&route).await? {
            return Err(IdentityError::Forbidden.into());
        }
        let principal = self.identity.resolve_channel(&route.identity).await?;
        self.store
            .menu_action(&route, principal, action, self.identity.now())
            .await?;
        let message = InboundMessage {
            route,
            event_id: event,
            text: format!("/卡片 {action}"),
            reply_kind: crate::gateway::ReplyKind::Event,
        };
        message.key()?;
        self.reap().await?;
        let mut owner = self.owner.lock();
        if owner.closing {
            return Err(GatewayError::Closing);
        }
        if owner.tasks.len() >= 128 {
            return Err(GatewayError::Busy);
        }
        let gateway = self.clone();
        owner.tasks.push(tokio::spawn(async move {
            let result = gateway.process(message).await;
            if matches!(
                result,
                Err(GatewayError::Database(_)
                    | GatewayError::Encoding(_)
                    | GatewayError::RecoveryRequired)
            ) {
                Err(GatewayError::RecoveryRequired)
            } else {
                Ok(())
            }
        }));
        Ok(())
    }

    pub(super) async fn menu_route(
        &self,
        message: &InboundMessage,
        principal: Uuid,
        devices: &[DeviceRecord],
        selected: Option<Uuid>,
        action: MenuAction,
    ) -> GatewayResult<DeliveryBody> {
        let mut title = "云 Agent 菜单".to_owned();
        let mut text =
            "选择一台执行设备后直接发送任务；未指定目录时使用该设备的默认工作区。".to_owned();
        let mut choices = Vec::new();
        let state = match selected {
            Some(id) => Some(self.runtime.journal().session(principal, id).await?),
            None => None,
        };
        let current = state
            .as_ref()
            .and_then(|s| devices.iter().find(|d| Some(d.id) == s.frozen.device_id));
        match action {
            MenuAction::Commands => return Ok(self.command_help("").into()),
            MenuAction::AggregateModels { page } => {
                return Ok(DeliveryBody::Menu {
                    card: self
                        .aggregate_model_menu(message, principal, selected, page)
                        .await?,
                })
            }
            MenuAction::ChooseAggregateModel { model, revision } => {
                return self
                    .choose_aggregate_model(message, selected, model, revision)
                    .await
            }

            MenuAction::Access {
                adapter,
                section,
                page,
            } => {
                return self
                    .access_menu(message, principal, adapter.as_deref(), &section, page)
                    .await
            }
            MenuAction::AccessChange {
                adapter,
                revision,
                change,
            } => {
                return self
                    .access_change(message, principal, &adapter, revision, &change)
                    .await
            }
            MenuAction::AccessInput {
                adapter,
                revision,
                list,
            } => {
                return self
                    .access_prompt(message, principal, &adapter, revision, list)
                    .await
            }
            MenuAction::Personal { section, page } => {
                return Ok(DeliveryBody::Menu {
                    card: self
                        .personal_menu(message, principal, selected, &section, page)
                        .await?,
                })
            }
            MenuAction::ClearPreferences { revision } => {
                return Ok(self.clear_preferences(principal, revision).await?.into())
            }
            MenuAction::MemoryChange { change } => {
                return Ok(self
                    .apply_memory_change(&message.route, principal, change)
                    .await?
                    .into());
            }
            MenuAction::MemoryPage { scope, page } => {
                return Ok(DeliveryBody::Menu {
                    card: self
                        .memory_scope_menu(message, principal, selected, page, scope)
                        .await?,
                });
            }
            MenuAction::QuestionPage {
                request,
                revision,
                question,
                offset,
            } => {
                return Ok(DeliveryBody::Menu {
                    card: self
                        .hub
                        .question_page(&message.route, request, revision, question, offset)
                        .await?,
                })
            }
            MenuAction::QuestionDecline { request, revision } => {
                return Ok(DeliveryBody::Menu {
                    card: self
                        .hub
                        .question_decline(&message.route, request, revision)
                        .await?,
                })
            }
            MenuAction::Home | MenuAction::Status => {
                if let (Some(state), Some(device)) = (&state, current) {
                    let activity = self
                        .activity(&message.route, principal, device, state)
                        .await?;
                    text = activity.text();
                    choices.extend([
                        ("工作区".into(), MenuAction::Workspaces),
                        ("工具权限".into(), MenuAction::Permissions),
                        ("状态".into(), MenuAction::Status),
                        ("取消任务".into(), MenuAction::Cancel),
                    ]);
                    if activity.has_result {
                        choices.push((
                            "查看结果".into(),
                            MenuAction::Results {
                                turn: None,
                                page: 1,
                            },
                        ));
                    }
                }
                choices.push(("命令帮助 /help".into(), MenuAction::Commands));
                choices.insert(0, ("选择设备".into(), MenuAction::Instances { offset: 0 }));
                choices.push((
                    "个人设置".into(),
                    MenuAction::Personal {
                        section: "home".into(),
                        page: 1,
                    },
                ));
                if self
                    .adapters
                    .get(&message.route.identity.adapter_instance_id)
                    .is_some_and(|a| {
                        a.conversation_scope(&message.route)
                            == crate::gateway::access::ConversationScope::Private
                    })
                    && self
                        .identity
                        .channel_is_admin(&message.route.identity)
                        .await?
                {
                    choices.push((
                        "网关与访问控制".into(),
                        MenuAction::Access {
                            adapter: None,
                            section: "home".into(),
                            page: 1,
                        },
                    ));
                }
            }
            MenuAction::Instances { offset } => {
                title = "选择执行设备".into();
                text = devices
                    .iter()
                    .skip(offset)
                    .take(6)
                    .enumerate()
                    .map(|(index, device)| {
                        format!(
                            "{}. {} · {}{}\n默认：{}",
                            offset + index + 1,
                            device.name,
                            device.platform,
                            if device.is_local {
                                " · 管理员专用"
                            } else {
                                " · 已登记"
                            },
                            device.default_workspace
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n");
                if text.is_empty() {
                    text = "尚未登记执行设备，请在执行器的登录入口关联账号。".into();
                }
                for device in devices.iter().skip(offset).take(6) {
                    choices.push((
                        device.name.clone(),
                        MenuAction::Select {
                            device: device.id,
                            workspace: None,
                        },
                    ));
                }
                if offset >= 6 {
                    choices.push((
                        "上一页".into(),
                        MenuAction::Instances { offset: offset - 6 },
                    ));
                }
                if offset + 6 < devices.len() {
                    choices.push((
                        "下一页".into(),
                        MenuAction::Instances { offset: offset + 6 },
                    ));
                }
                choices.push(("返回菜单".into(), MenuAction::Home));
            }
            MenuAction::Workspaces => {
                return self
                    .menu_route_boxed(
                        message,
                        principal,
                        devices,
                        selected,
                        MenuAction::WorkspacesPage { offset: 0 },
                    )
                    .await;
            }
            MenuAction::WorkspacesPage { offset } => {
                let (Some(state), Some(device)) = (&state, current) else {
                    return self
                        .menu_route_boxed(
                            message,
                            principal,
                            devices,
                            selected,
                            MenuAction::Instances { offset: 0 },
                        )
                        .await;
                };
                title = "选择工作区".into();
                let paths: Vec<_> = self
                    .store
                    .known_workspaces(principal, device.id)
                    .await?
                    .into_iter()
                    .filter(|p| *p != state.frozen.workspace() && *p != device.default_workspace)
                    .collect();
                text = format!("设备：{}\n当前：{}\n默认（全局）：{}\n\n历史工作区第 {}/{} 页，共 {} 个。\n可直接使用默认目录，或浏览实例上的文件夹。切换后开始新会话，保留当前权限与旧历史。",device.name,state.frozen.workspace(),device.default_workspace,offset / 6 + 1, paths.len().div_ceil(6).max(1),paths.len());
                choices.push((
                    "使用默认工作区".into(),
                    MenuAction::Select {
                        device: device.id,
                        workspace: None,
                    },
                ));
                choices.push((
                    "浏览当前目录".into(),
                    MenuAction::Browse {
                        device: device.id,
                        path: Some(state.frozen.workspace().clone()),
                        offset: 0,
                    },
                ));
                choices.push((
                    "系统根 / 磁盘".into(),
                    MenuAction::Browse {
                        device: device.id,
                        path: None,
                        offset: 0,
                    },
                ));
                for path in paths.iter().skip(offset).take(6) {
                    choices.push((
                        path.clone(),
                        MenuAction::Select {
                            device: device.id,
                            workspace: Some(path.clone()),
                        },
                    ));
                }
                if offset >= 6 {
                    choices.push((
                        "上一页工作区".into(),
                        MenuAction::WorkspacesPage { offset: offset - 6 },
                    ));
                }
                if offset + 6 < paths.len() {
                    choices.push((
                        "下一页工作区".into(),
                        MenuAction::WorkspacesPage { offset: offset + 6 },
                    ));
                }
                choices.push(("返回菜单".into(), MenuAction::Home));
            }
            MenuAction::Results { turn, page } => {
                title = "已保存的任务结果".into();
                if let Some(result) = self
                    .saved_result(&message.route, principal, page, turn)
                    .await?
                {
                    text = result.text();
                    if page > 1 {
                        choices.push((
                            "上一页".into(),
                            MenuAction::Results {
                                turn: Some(result.turn_id),
                                page: page - 1,
                            },
                        ));
                    }
                    if page < result.page_count {
                        choices.push((
                            "下一页".into(),
                            MenuAction::Results {
                                turn: Some(result.turn_id),
                                page: page + 1,
                            },
                        ));
                    }
                } else {
                    text = "这个聊天还没有已保存的任务结果。".into();
                }
                choices.push(("返回菜单".into(), MenuAction::Home));
            }
            MenuAction::QuestionOption {
                request,
                revision,
                question,
                option,
                selected,
            } => {
                return Ok(DeliveryBody::Menu {
                    card: self
                        .hub
                        .question_option(
                            &message.route,
                            request,
                            revision,
                            question,
                            option,
                            selected,
                        )
                        .await?,
                });
            }
            MenuAction::QuestionNext {
                request,
                revision,
                question,
            } => {
                return Ok(DeliveryBody::Menu {
                    card: self
                        .hub
                        .question_next(&message.route, request, revision, question)
                        .await?,
                });
            }
            MenuAction::Browse {
                device: id,
                path,
                offset,
            } => {
                let device = devices
                    .iter()
                    .find(|d| d.id == id)
                    .ok_or(IdentityError::Forbidden)?;
                let listing = self
                    .connector
                    .directories(
                        principal,
                        device,
                        DirectoryQuery {
                            path: path.clone().unwrap_or_default(),
                            offset: if path.is_some() { offset } else { 0 },
                            limit: 6,
                        },
                    )
                    .await?;
                title = "浏览实例目录".into();
                if path.is_none() {
                    text = format!("设备：{}\n选择系统根目录或磁盘。", device.name);
                    for root in listing.roots.iter().skip(offset).take(8) {
                        choices.push((
                            root.clone(),
                            MenuAction::Browse {
                                device: id,
                                path: Some(root.clone()),
                                offset: 0,
                            },
                        ));
                    }
                    if offset + 8 < listing.roots.len() {
                        choices.push((
                            "更多磁盘".into(),
                            MenuAction::Browse {
                                device: id,
                                path: None,
                                offset: offset + 8,
                            },
                        ));
                    }
                    choices.push((
                        "默认目录".into(),
                        MenuAction::Browse {
                            device: id,
                            path: Some(device.default_workspace.clone()),
                            offset: 0,
                        },
                    ));
                } else {
                    text = format!(
                        "设备：{}\n目录：{}\n\n{}",
                        device.name,
                        listing.path,
                        if listing.directories.is_empty() {
                            "此目录没有子文件夹。"
                        } else {
                            "点击文件夹继续浏览；点击“使用此目录”才会切换工作区。"
                        }
                    );
                    for child in listing.directories {
                        choices.push((
                            child.name,
                            MenuAction::Browse {
                                device: id,
                                path: Some(child.path),
                                offset: 0,
                            },
                        ));
                    }
                    choices.push((
                        "使用此目录".into(),
                        MenuAction::Select {
                            device: id,
                            workspace: Some(listing.path.clone()),
                        },
                    ));
                    choices.push((
                        "设为全局默认".into(),
                        MenuAction::SetDefault {
                            device: id,
                            workspace: listing.path.clone(),
                        },
                    ));
                    if let Some(parent) = listing.parent {
                        choices.push((
                            "上一级".into(),
                            MenuAction::Browse {
                                device: id,
                                path: Some(parent),
                                offset: 0,
                            },
                        ));
                    }
                    if let Some(next) = listing.next_offset {
                        choices.push((
                            "下一页目录".into(),
                            MenuAction::Browse {
                                device: id,
                                path: Some(listing.path),
                                offset: next,
                            },
                        ));
                    }
                    choices.push((
                        "系统根 / 磁盘".into(),
                        MenuAction::Browse {
                            device: id,
                            path: None,
                            offset: 0,
                        },
                    ));
                }
                choices.push(("返回菜单".into(), MenuAction::Home));
            }
            MenuAction::Select { device, workspace } => {
                let device = devices
                    .iter()
                    .find(|d| d.id == device)
                    .ok_or(IdentityError::Forbidden)?;
                let frozen = self
                    .select_instance(
                        &message.route,
                        principal,
                        device,
                        selected,
                        workspace.as_deref(),
                    )
                    .await?;
                title = "设备与工作区已就绪".into();
                text = format!(
                    "设备：{}\n工作区：{}\n现在直接发送任务即可。",
                    device.name,
                    frozen.workspace()
                );
                choices.extend([
                    ("选择工作区".into(), MenuAction::Workspaces),
                    ("返回菜单".into(), MenuAction::Home),
                ]);
            }
            MenuAction::SetDefault { device, workspace } => {
                let device = devices
                    .iter()
                    .find(|d| d.id == device)
                    .ok_or(IdentityError::Forbidden)?;
                let listing = self
                    .connector
                    .directories(
                        principal,
                        device,
                        DirectoryQuery {
                            path: workspace,
                            offset: 0,
                            limit: 1,
                        },
                    )
                    .await?;
                self.identity
                    .set_instance_default(principal, device.id, &listing.path)
                    .await?;
                title = "默认工作区已更新".into();
                text=format!("设备：{}\n默认（全局）：{}\n以后选择此实例且不指定路径时使用这个目录。当前会话不变。",device.name,listing.path);
                choices.extend([
                    (
                        "使用默认工作区".into(),
                        MenuAction::Select {
                            device: device.id,
                            workspace: None,
                        },
                    ),
                    ("返回菜单".into(), MenuAction::Home),
                ]);
            }
            MenuAction::Permissions => {
                let Some(state) = state else {
                    return Err(GatewayError::Invalid);
                };
                title = "当前会话工具权限".into();
                text=format!("当前：{}\n默认：按现有工具策略审批\n编辑：使用 Peri 编辑权限模式\n全部：本会话允许全部工具",permission_label(state.permissions()?));
                for mode in ["默认", "编辑", "全部"] {
                    choices.push((mode.into(), MenuAction::SetPermission { mode: mode.into() }));
                }
                choices.push(("返回菜单".into(), MenuAction::Home));
            }
            MenuAction::SetPermission { mode } => {
                let Some(session) = selected else {
                    return Err(GatewayError::Invalid);
                };
                let mode = match mode.as_str() {
                    "默认" => PermissionMode::Default,
                    "编辑" => PermissionMode::AcceptEdit,
                    "全部" => PermissionMode::Bypass,
                    _ => return Err(GatewayError::Invalid),
                };
                self.runtime
                    .journal()
                    .set_permissions(principal, session, mode)
                    .await?;
                title = "权限已更新".into();
                text = format!("当前会话权限：{}", permission_label(mode));
                choices.push(("返回菜单".into(), MenuAction::Home));
            }
            MenuAction::Cancel => {
                let Some(session) = selected else {
                    return Err(GatewayError::Invalid);
                };
                if let Some(turn) = self
                    .runtime
                    .journal()
                    .active_turn(principal, session)
                    .await?
                {
                    self.runtime
                        .cancel(principal, session, turn.turn_id)
                        .await?;
                    text = "已提交取消请求，执行器确认后任务才算结束。".into();
                } else {
                    text = "当前没有正在运行的云端任务。".into();
                }
                title = "任务状态".into();
                choices.push(("返回菜单".into(), MenuAction::Home));
            }
        }
        self.build_menu(message, principal, title, text, choices)
            .await
    }

    fn menu_route_boxed<'a>(
        &'a self,
        message: &'a InboundMessage,
        principal: Uuid,
        devices: &'a [DeviceRecord],
        selected: Option<Uuid>,
        action: MenuAction,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = GatewayResult<DeliveryBody>> + Send + 'a>>
    {
        Box::pin(self.menu_route(message, principal, devices, selected, action))
    }

    pub(in crate::gateway) async fn build_menu(
        &self,
        message: &InboundMessage,
        principal: Uuid,
        title: String,
        mut text: String,
        choices: Vec<(String, MenuAction)>,
    ) -> GatewayResult<DeliveryBody> {
        let expected_session = self.store.selection(&message.route, principal).await?;
        if matches!(title.as_str(), "云 Agent 菜单" | "任务状态" | "工作区") {
            if let Some(url) = self.control_link(expected_session) {
                text.push_str(&format!("\n复杂任务与完整状态（需登录）：{url}"));
            }
        }
        let expires_at = self.identity.now() + 240;
        if choices.len() > 15 {
            return Err(GatewayError::Invalid);
        }
        let mut buttons = Vec::new();
        for (label, action) in choices {
            let action_id = self
                .store
                .save_menu_action(
                    &message.route,
                    principal,
                    MenuRecord {
                        action,
                        expected_session,
                        expires_at,
                    },
                )
                .await?;
            buttons.push(MenuButton { label, action_id });
        }
        Ok(DeliveryBody::Menu {
            card: MenuCard {
                question_request: None,
                title,
                text,
                buttons,
            },
        })
    }
}
