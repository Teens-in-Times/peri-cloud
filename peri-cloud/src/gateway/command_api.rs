//! Formal command interface; HTTP is not a fabricated QQ event.
use super::*;
use crate::commands::CommandDescriptor;
use crate::identity::Authenticated;
use serde::Serialize;

#[derive(Serialize)]
pub struct CommandReceipt {
    pub request_id: Uuid,
    pub session_id: Uuid,
    pub state: &'static str,
    pub text: Option<String>,
    pub task: Option<TaskReceipt>,
}
impl Gateway {
    pub fn command_catalog(&self) -> Vec<CommandDescriptor> {
        let registry = self
            .connector
            .command_registry()
            .unwrap_or_else(crate::commands::native_registry);
        let mut catalog = crate::commands::catalog(&registry);
        for (name, description) in [
            ("new", "当前设备和目录开启新会话"),
            ("help", "完整命令帮助"),
            ("pwd", "会话和工作区信息"),
            (
                "schedule",
                "持久定时任务：一次、周期、暂停、恢复、删除与触发记录",
            ),
        ] {
            if catalog
                .iter()
                .any(|entry| entry.name == format!("core:{name}"))
            {
                continue;
            }
            catalog.push(CommandDescriptor {
                name: format!("core:{name}"),
                aliases: Vec::new(),
                description: description.into(),
                args: None,
                availability: "available",
                reason: None,
            });
        }
        catalog
    }
    pub async fn execute_browser_command(
        self: &Arc<Self>,
        actor: &Authenticated,
        session: Uuid,
        request: Uuid,
        input: String,
    ) -> GatewayResult<CommandReceipt> {
        self.identity.browser_scope(actor).await?;
        self.journal()
            .session(actor.principal().id, session)
            .await
            .map_err(|error| match error {
                StateError::Forbidden | StateError::NotFound => {
                    GatewayError::Identity(IdentityError::Forbidden)
                }
                other => other.into(),
            })?;
        if input.len() > self.limits().max_prompt_bytes || !input.trim().starts_with('/') {
            return Err(GatewayError::Invalid);
        }
        let text = commands::normalize(&input);
        if let Some(registry) = self.connector.command_registry() {
            if let Some(command) = registry
                .resolve(input.trim())
                .or_else(|| registry.resolve(&text))
            {
                if command.entry.args_schema.as_ref().is_some_and(|schema| {
                    crate::commands::parse_arguments(schema, &command.args).is_err()
                }) {
                    return Err(GatewayError::Invalid);
                }
                if command.entry.fullname == "core:loop" && command.args.is_empty() {
                    return Err(GatewayError::Invalid);
                }
                let text = if command.args.is_empty() {
                    format!("/{}", command.entry.fullname)
                } else {
                    format!("/{} {}", command.entry.fullname, command.args)
                };
                let task = self
                    .submit_browser_task(actor, session, request, text)
                    .await?;
                return Ok(CommandReceipt {
                    request_id: request,
                    session_id: session,
                    state: "accepted",
                    text: None,
                    task: Some(task),
                });
            }
        }
        self.reap().await?;
        let (sender, receiver) = oneshot::channel();
        {
            let mut owner = self.owner.lock();
            if owner.closing {
                return Err(GatewayError::Closing);
            }
            if owner.tasks.len() >= 128 {
                return Err(GatewayError::Busy);
            }
            let gateway = self.clone();
            let actor = actor.clone();
            owner.tasks.push(tokio::spawn(async move {
                let result = gateway
                    .browser_command_owned(&actor, session, request, &text)
                    .await;
                let uncertain = matches!(
                    &result,
                    Err(GatewayError::Database(_)
                        | GatewayError::Encoding(_)
                        | GatewayError::RecoveryRequired)
                );
                let _ = sender.send(result);
                if uncertain {
                    Err(GatewayError::RecoveryRequired)
                } else {
                    Ok(())
                }
            }));
        }
        receiver.await.map_err(|_| GatewayError::RecoveryRequired)?
    }
    async fn browser_command_owned(
        &self,
        actor: &Authenticated,
        session: Uuid,
        request: Uuid,
        text: &str,
    ) -> GatewayResult<CommandReceipt> {
        use sha2::{Digest, Sha256};
        let principal = actor.principal().id;
        let gate = self.account_gate(principal.to_string());
        let _guard = gate.lock().await;
        self.identity.browser_scope(actor).await?;
        let hash = (Sha256::digest(serde_json::to_vec(&(session, text))?))
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let saved = sqlx::query_as::<_, (String, String, String, String, String)>(
            "SELECT principal,request_hash,state,result_json,kind FROM control_operations WHERE id=?",
        )
        .bind(request.to_string())
        .fetch_optional(&self.store.pool)
        .await?;
        if let Some((owner, expected, state, result, kind)) = saved {
            if owner != principal.to_string() || expected != hash || kind != "command.execute" {
                return Err(StateError::InvocationConflict.into());
            }
            if state == "failed" {
                return Err(GatewayError::Invalid);
            }
            if state != "succeeded" {
                return Err(GatewayError::RecoveryRequired);
            }
            let value: serde_json::Value = serde_json::from_str(&result)?;
            return Ok(CommandReceipt {
                request_id: request,
                session_id: serde_json::from_value(value["session_id"].clone())?,
                state: if value["state"] == "unsupported" {
                    "unsupported"
                } else if value["state"] == "interactive" {
                    "interactive"
                } else {
                    "completed"
                },
                text: value["text"].as_str().map(str::to_owned),
                task: None,
            });
        }
        let route = self.browser_task_route(actor, session).await?;
        let state = self.journal().session(principal, session).await?;
        let (_, devices) = self.identity.channel_devices(&route.identity).await?;
        let device = devices
            .iter()
            .find(|d| Some(d.id) == state.frozen.device_id);
        if state.frozen.device_id.is_some() && device.is_none() {
            return Err(IdentityError::Forbidden.into());
        }
        let interactive = match text {
            "/模型" => Some("model-panel"),
            "/设置" | "/记忆" | "/用量" => Some("account-settings"),
            "/菜单" => Some("activity-panel"),
            "/网关" => Some("admin-panel"),
            "/设备" | "/工作区" => Some("device-panel"),
            _ => None,
        };
        if text == "/网关" {
            self.identity.require_admin(actor).await?;
        }
        let supported = text == "/new"
            || text == "/help"
            || text.starts_with("/help ")
            || text == "/pwd"
            || text == "/状态"
            || text == "/取消"
            || text == "/定时"
            || text.starts_with("/定时 ")
            || text == "/threads"
            || text.starts_with("/threads ")
            || text.starts_with("/resume ")
            || self.unavailable_command(text).is_some()
            || interactive.is_some();
        if !supported {
            return Err(GatewayError::Invalid);
        }
        sqlx::query("INSERT INTO control_operations(id,principal,kind,target,request_hash,state,result_json,created_at) VALUES(?,?,'command.execute',?,?,'pending','{}',?)")
            .bind(request.to_string()).bind(principal.to_string()).bind(session.to_string()).bind(hash).bind(self.identity.now()).execute(&self.store.pool).await?;
        let result: GatewayResult<String> = if let Some(section) = interactive {
            let link = self.control_link(Some(session)).and_then(|v| {
                v.split_once('#')
                    .map(|(base, _)| format!("{base}#{section}"))
            });
            Ok(format!(
                "此命令是原生界面的交互面板。继续使用同一后端的结构化入口：{}",
                link.unwrap_or_else(|| format!("工作台 #{section}"))
            ))
        } else if text == "/定时" || text.starts_with("/定时 ") {
            self.schedule_command(&route, principal, Some(session), text, request.to_string())
                .await
        } else if text == "/状态" {
            Ok(self
                .conversation_activity(&route, principal, device, &state)
                .await?
                .text())
        } else if text == "/取消" {
            if let Some(turn) = self.journal().active_turn(principal, session).await? {
                self.runtime
                    .cancel(principal, session, turn.turn_id)
                    .await?;
                Ok("已提交取消请求，请核对任务状态。".into())
            } else {
                Ok("没有正在运行的云任务。".into())
            }
        } else if text == "/new" {
            match device {
                Some(device) => {
                    self.new_chat_session(&route, principal, device, session)
                        .await
                }
                None => self
                    .new_cloud_conversation(&route, principal, session)
                    .await
                    .map(|id| format!("已开始空白云端会话：{id}。")),
            }
        } else if text == "/help" || text.starts_with("/help ") {
            Ok(self.command_help(text.strip_prefix("/help").unwrap().trim()))
        } else if text == "/pwd" {
            Ok(format!(
                "会话：{session}\n设备：{}\n目录：{}",
                device.map(|d| d.name.as_str()).unwrap_or("云端对话"),
                state.frozen.workspace()
            ))
        } else if let Some(reason) = self.unavailable_command(text) {
            Ok(reason)
        } else {
            self.history_control(&route, principal, session, text).await
        };
        let unavailable = self.unavailable_command(text).is_some();
        match result {
            Ok(text) => {
                let selected = self
                    .store
                    .selection(&route, principal)
                    .await?
                    .ok_or(GatewayError::RecoveryRequired)?;
                let response_state = if unavailable {
                    "unsupported"
                } else {
                    "completed"
                };
                let response_state = if interactive.is_some() {
                    "interactive"
                } else {
                    response_state
                };
                let value =
                    serde_json::json!({"session_id":selected,"text":text,"state":response_state});
                sqlx::query(
                    "UPDATE control_operations SET state='succeeded',result_json=? WHERE id=?",
                )
                .bind(value.to_string())
                .bind(request.to_string())
                .execute(&self.store.pool)
                .await?;
                Ok(CommandReceipt {
                    request_id: request,
                    session_id: selected,
                    state: response_state,
                    text: Some(text),
                    task: None,
                })
            }
            Err(error) => {
                let uncertain = matches!(
                    &error,
                    GatewayError::Database(_)
                        | GatewayError::Encoding(_)
                        | GatewayError::RecoveryRequired
                );
                sqlx::query("UPDATE control_operations SET state=? WHERE id=?")
                    .bind(if uncertain { "unknown" } else { "failed" })
                    .bind(request.to_string())
                    .execute(&self.store.pool)
                    .await?;
                Err(error)
            }
        }
    }
}
