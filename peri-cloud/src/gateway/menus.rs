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
        let mut text = "选择实例后直接发送任务；未指定目录时使用实例的默认工作区。".to_owned();
        let mut choices = Vec::new();
        let state = match selected {
            Some(id) => Some(self.runtime.journal().session(principal, id).await?),
            None => None,
        };
        let current = state
            .as_ref()
            .and_then(|s| devices.iter().find(|d| d.id == s.frozen.device_id));
        match action {
            MenuAction::Home | MenuAction::Status => {
                if let (Some(state), Some(device)) = (&state, current) {
                    let busy = !state.execution_settled()
                        || self
                            .runtime
                            .journal()
                            .has_unconfirmed_turn(state.frozen.session_id)
                            .await?;
                    text = format!(
                        "实例：{}\n工作区：{}\n默认工作区：{}\n权限：{}\n状态：{}",
                        device.name,
                        state.frozen.binding.workspace,
                        device.default_workspace,
                        permission_label(state.permissions()?),
                        if busy {
                            "运行中或待确认"
                        } else {
                            "空闲"
                        }
                    );
                    choices.extend([
                        ("工作区".into(), MenuAction::Workspaces),
                        ("工具权限".into(), MenuAction::Permissions),
                        ("状态".into(), MenuAction::Status),
                        ("取消任务".into(), MenuAction::Cancel),
                    ]);
                }
                choices.insert(0, ("选择实例".into(), MenuAction::Instances { offset: 0 }));
            }
            MenuAction::Instances { offset } => {
                title = "选择执行实例".into();
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
                                " · 服务器本地"
                            } else {
                                " · 已登记"
                            },
                            device.default_workspace
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n");
                if text.is_empty() {
                    text = "尚未登记执行实例，请在执行器的登录入口关联账号。".into();
                }
                for (index, device) in devices.iter().skip(offset).take(6).enumerate() {
                    choices.push((
                        format!("{} · {}", offset + index + 1, device.name),
                        MenuAction::Select {
                            device: device.id,
                            workspace: None,
                        },
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
                let paths = self.store.known_workspaces(principal, device.id).await?;
                text = format!("实例：{}\n当前：{}\n默认（全局）：{}\n\n已有工作区：\n{}\n\n可直接使用默认目录，或浏览实例上的文件夹。切换后开始新会话，保留当前权限与旧历史。",device.name,state.frozen.binding.workspace,device.default_workspace,paths.iter().take(8).cloned().collect::<Vec<_>>().join("\n"));
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
                        path: Some(state.frozen.binding.workspace.clone()),
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
                for path in paths
                    .iter()
                    .filter(|p| {
                        **p != state.frozen.binding.workspace && **p != device.default_workspace
                    })
                    .take(7)
                {
                    choices.push((
                        folder_label(path),
                        MenuAction::Select {
                            device: device.id,
                            workspace: Some(path.clone()),
                        },
                    ));
                }
                choices.push(("返回菜单".into(), MenuAction::Home));
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
                    text = format!("实例：{}\n选择系统根目录或磁盘。", device.name);
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
                        "实例：{}\n目录：{}\n\n{}",
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
                title = "实例与工作区已就绪".into();
                text = format!(
                    "实例：{}\n工作区：{}\n现在直接发送任务即可。",
                    device.name, frozen.binding.workspace
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
                text=format!("实例：{}\n默认（全局）：{}\n以后选择此实例且不指定路径时使用这个目录。当前会话不变。",device.name,listing.path);
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

    async fn build_menu(
        &self,
        message: &InboundMessage,
        principal: Uuid,
        title: String,
        text: String,
        choices: Vec<(String, MenuAction)>,
    ) -> GatewayResult<DeliveryBody> {
        let expected_session = self.store.selection(&message.route, principal).await?;
        let expires_at = self.identity.now() + 240;
        let mut buttons = Vec::new();
        for (label, action) in choices.into_iter().take(15) {
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
                title,
                text,
                buttons,
            },
        })
    }
}

fn folder_label(path: &str) -> String {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
        .into()
}
