use super::*;
use peri_acp_types::messages::BaseMessage;

async fn command(f: &Fixture, event: &str, text: &str) -> peri_cloud::gateway::GatewayReceipt {
    f.c().gateway.receive(f.message(event, text)).await.unwrap()
}
async fn selected(f: &Fixture) -> Uuid {
    f.c()
        .gateway
        .browser_workspaces(&f.c().browser)
        .await
        .unwrap()
        .remove(0)
        .session_id
}
async fn state(f: &Fixture, id: Uuid) -> peri_cloud::state::SessionState {
    f.c()
        .journal
        .session(f.c().browser.principal().id, id)
        .await
        .unwrap()
}

#[tokio::test]
async fn new_keeps_device_directory_permissions_history_and_deduplicates() {
    let mut f = Fixture::new(vec![answer("previous private text")]).await;
    f.pair().await;
    f.select("select", 0).await;
    command(&f, "permissions", "/permissions all").await;
    let turn = command(&f, "task", "remember the previous task").await;
    let old = turn.session_id.unwrap();
    f.settled(old, turn.turn_id.unwrap()).await;
    let before = state(&f, old).await;
    let message = f.message("new", "/new");
    f.c().gateway.receive(message.clone()).await.unwrap();
    let new = selected(&f).await;
    assert_ne!(old, new);
    f.c().gateway.receive(message).await.unwrap();
    assert_eq!(new, selected(&f).await);
    let after = state(&f, new).await;
    assert!(after.history.is_empty());
    assert_eq!(
        before.frozen.binding.workspace,
        after.frozen.binding.workspace
    );
    assert_eq!(before.permission_mode, after.permission_mode);
    assert_eq!(state(&f, old).await.history.len(), before.history.len());
    assert_eq!(f.script.requests.lock().len(), 1);
    f.restart().await;
    assert_eq!(selected(&f).await, new);
    command(&f, "resume", &format!("/resume {old}")).await;
    assert_eq!(selected(&f).await, old);
    f.finish().await;
}

#[tokio::test]
async fn compact_uses_summary_only_no_tools_keeps_canonical_history_and_survives_restart() {
    let mut f = Fixture::new(vec![
        answer("OLD_ASSISTANT_CONTENT"),
        answer("<summary>SUMMARY_ONLY_FACT</summary>"),
        answer("next reply"),
    ])
    .await;
    f.pair().await;
    f.select("select", 0).await;
    let turn = command(&f, "task", "OLD_USER_CONTENT").await;
    let session = turn.session_id.unwrap();
    f.settled(session, turn.turn_id.unwrap()).await;
    let before = state(&f, session).await;
    let message = f.message("compact", "/compact");
    let compact = f.c().gateway.receive(message.clone()).await.unwrap();
    f.settled(session, compact.turn_id.unwrap()).await;
    let record = f
        .c()
        .journal
        .turn(
            f.c().browser.principal().id,
            session,
            compact.turn_id.unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(record.state, TurnState::Completed);
    let after = state(&f, session).await;
    assert_eq!(after.history.len(), before.history.len() + 1);
    for payload in &before.history {
        assert!(after.history_flags[&payload.id()].excluded);
        assert!(after.history.iter().any(|p| p.id() == payload.id()));
    }
    let summary_request = f.script.requests.lock()[1].clone();
    assert!(summary_request
        .get("tools")
        .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty)));
    assert!(summary_request.to_string().contains("OLD_USER_CONTENT"));
    f.c().gateway.receive(message).await.unwrap();
    assert_eq!(f.script.requests.lock().len(), 2);
    f.restart().await;
    let next = command(&f, "next", "continue").await;
    f.settled(session, next.turn_id.unwrap()).await;
    let request = f.script.requests.lock()[2].to_string();
    assert!(request.contains("SUMMARY_ONLY_FACT"));
    assert!(!request.contains("OLD_USER_CONTENT"));
    assert!(!request.contains("OLD_ASSISTANT_CONTENT"));
    f.finish().await;
}

#[tokio::test]
async fn failed_compact_and_busy_controls_leave_context_and_selection_unchanged() {
    let f = Fixture::new(vec![
        answer("previous"),
        answer("   "),
        tool_write(),
        answer("denied"),
    ])
    .await;
    f.pair().await;
    f.select("select", 0).await;
    let task = command(&f, "first", "first").await;
    let session = task.session_id.unwrap();
    f.settled(session, task.turn_id.unwrap()).await;
    let before = state(&f, session).await;
    let compact = command(&f, "failedcompact", "/compact").await;
    f.settled(session, compact.turn_id.unwrap()).await;
    let after = state(&f, session).await;
    assert_eq!(after.history.len(), before.history.len());
    assert_eq!(
        serde_json::to_value(after.history_flags).unwrap(),
        serde_json::to_value(before.history_flags).unwrap()
    );
    let running = command(&f, "write", "write").await;
    let card = f.card().await;
    command(&f, "busynew", "/new").await;
    command(&f, "busycompact", "/compact").await;
    assert_eq!(selected(&f).await, session);
    assert_eq!(f.script.requests.lock().len(), 3);
    assert!(!f.workspaces[0].path().join("agent.txt").exists());
    f.c()
        .gateway
        .respond(&f.route, card.request_id, InteractionAction::Reject {})
        .await
        .unwrap();
    f.settled(session, running.turn_id.unwrap()).await;
    f.finish().await;
}

#[tokio::test]
async fn clear_and_rewind_change_context_without_models_or_file_side_effects() {
    let f = Fixture::new(vec![answer("one"), answer("two"), answer("after clear")]).await;
    f.pair().await;
    f.select("select", 0).await;
    let first = command(&f, "first", "first prompt").await;
    let session = first.session_id.unwrap();
    f.settled(session, first.turn_id.unwrap()).await;
    let second = command(&f, "second", "second prompt").await;
    f.settled(session, second.turn_id.unwrap()).await;
    let before = state(&f, session).await;
    let target = before
        .history
        .iter()
        .filter_map(|p| p.as_message())
        .find(|m| matches!(m, BaseMessage::Human { .. }) && m.content() == "second prompt")
        .unwrap()
        .id()
        .as_uuid();
    let unsupported = command(&f, "file-rewind", &format!("/rewind {target}")).await;
    f.settled(session, unsupported.turn_id.unwrap()).await;
    assert_eq!(
        f.c()
            .journal
            .turn(
                f.c().browser.principal().id,
                session,
                unsupported.turn_id.unwrap()
            )
            .await
            .unwrap()
            .state,
        TurnState::Failed
    );
    let rewind = command(
        &f,
        "rewind",
        &format!("/core:rewind {target} --no-revert-files"),
    )
    .await;
    f.settled(session, rewind.turn_id.unwrap()).await;
    assert!(state(&f, session)
        .await
        .history_flags
        .iter()
        .any(|(id, flag)| id.as_uuid() == target && flag.excluded));
    let clear = command(&f, "clear", "/reset").await;
    f.settled(session, clear.turn_id.unwrap()).await;
    assert_eq!(f.script.requests.lock().len(), 2);
    assert_eq!(state(&f, session).await.history.len(), before.history.len());
    let next = command(&f, "next", "new prompt").await;
    f.settled(session, next.turn_id.unwrap()).await;
    let request = f.script.requests.lock()[2].to_string();
    assert!(!request.contains("first prompt"));
    assert!(!request.contains("second prompt"));
    f.finish().await;
}

#[tokio::test]
async fn formal_command_api_checks_owner_deduplicates_new_and_exposes_true_capabilities() {
    let f = Fixture::new_for(vec![], true).await;
    let c = f.c();
    let session = c
        .gateway
        .open_browser_workspace(&c.browser, Uuid::new_v4(), f.devices[0].id, None)
        .await
        .unwrap();
    let request = Uuid::new_v4();
    let receipt = c
        .gateway
        .execute_browser_command(&c.browser, session, request, "/new".into())
        .await
        .unwrap();
    assert_ne!(receipt.session_id, session);
    assert_eq!(
        c.gateway
            .execute_browser_command(&c.browser, session, request, "/new".into())
            .await
            .unwrap()
            .session_id,
        receipt.session_id
    );
    assert!(c
        .gateway
        .execute_browser_command(&c.browser, session, request, "/pwd".into())
        .await
        .is_err());
    let invite = c.identity.create_invite(&c.browser).await.unwrap();
    c.identity
        .register_invited(&invite.code, "guest-command", PASSWORD.into(), "Guest")
        .await
        .unwrap();
    let login = c
        .identity
        .login("guest-command", PASSWORD.into())
        .await
        .unwrap();
    let guest = c
        .identity
        .authenticate_browser(&login.session_token)
        .await
        .unwrap();
    assert!(c
        .gateway
        .execute_browser_command(&guest, receipt.session_id, Uuid::new_v4(), "/clear".into())
        .await
        .is_err());
    let catalog = c.gateway.command_catalog();
    assert!(catalog
        .iter()
        .any(|c| c.name == "core:compact" && c.availability == "available"));
    assert!(catalog
        .iter()
        .any(|c| c.name == "ui:mcp" && c.availability == "unavailable"));
    let disabled = c
        .gateway
        .execute_browser_command(
            &c.browser,
            receipt.session_id,
            Uuid::new_v4(),
            "/ui:mcp".into(),
        )
        .await
        .unwrap();
    assert_eq!(disabled.state, "unsupported");
    assert!(f.script.requests.lock().is_empty());
    f.finish().await;
}

struct RegisteredCommand {
    wait: bool,
    session: bool,
}
#[async_trait]
impl peri_acp_types::command::CommandHandler for RegisteredCommand {
    async fn execute(
        &self,
        context: peri_acp_types::command::CommandContext,
    ) -> peri_acp_types::command::CommandOutcome {
        use peri_acp_types::command::*;
        let scope = context
            .dep::<peri_cloud::commands::CloudCommandScope>()
            .expect("authenticated scope");
        assert_eq!(scope.session_id.to_string(), context.session_id);
        if self.wait {
            context.cancel_token.cancelled().await;
        }
        CommandOutcome::Done(CommandResult {
            messages: context.history,
            stop_reason: if self.wait {
                PromptStopReason::Cancelled
            } else {
                PromptStopReason::EndTurn
            },
            feedback: Some(CommandFeedback {
                level: FeedbackLevel::Info,
                message: "REGISTERED_COMMAND_DONE".into(),
                channel: if self.session {
                    FeedbackChannel::Session
                } else {
                    FeedbackChannel::UiOnly
                },
            }),
        })
    }
}
fn registered(name: &str, wait: bool) -> peri_acp_types::command::command_route::RouteEntry {
    use peri_acp_types::command::command_route::*;
    RouteEntry {
        fullname: format!("plugin:fixture:{name}"),
        aliases: Vec::new(),
        description: "fixture native handler".into(),
        kind: CommandEntryKind::Command,
        category: None,
        args_schema: Some(Default::default()),
        handler: Arc::new(RegisteredCommand {
            wait,
            session: false,
        }),
        provenance: CommandProvenance {
            source: CommandSource::Plugin {
                name: "fixture".into(),
            },
            lifecycle: CommandLifecycle::Connected,
        },
    }
}
#[tokio::test]
async fn native_handler_registration_loop_and_cancellation_use_the_existing_runtime() {
    let f = Fixture::new(vec![answer("goal done")]).await;
    f.pair().await;
    f.select("select", 0).await;
    f.connector
        .register_command(registered("ping", false))
        .unwrap();
    assert!(f
        .connector
        .register_command(registered("ping", false))
        .is_err());
    let ping = command(&f, "ping", "/plugin:fixture:ping").await;
    let session = ping.session_id.unwrap();
    f.settled(session, ping.turn_id.unwrap()).await;
    assert!(f.script.requests.lock().is_empty());
    let receipt = f
        .c()
        .journal
        .turn(f.c().browser.principal().id, session, ping.turn_id.unwrap())
        .await
        .unwrap();
    assert_eq!(receipt.replies[0].text, "REGISTERED_COMMAND_DONE");
    let task = command(&f, "loop", "/loop finish the test goal").await;
    f.settled(session, task.turn_id.unwrap()).await;
    assert!(f.script.requests.lock()[0]
        .to_string()
        .contains("Loop: finish the test goal"));
    f.connector
        .register_command(registered("wait", true))
        .unwrap();
    let waiting = command(&f, "wait", "/plugin:fixture:wait").await;
    command(&f, "cancel", "/cancel").await;
    f.settled(session, waiting.turn_id.unwrap()).await;
    assert_eq!(
        f.c()
            .journal
            .turn(
                f.c().browser.principal().id,
                session,
                waiting.turn_id.unwrap()
            )
            .await
            .unwrap()
            .state,
        TurnState::Interrupted
    );
    f.finish().await;
}

#[tokio::test]
async fn command_http_routes_enforce_cookie_csrf_and_confirmation() {
    let f = Fixture::new_for(vec![], true).await;
    let c = f.c();
    let session = c
        .gateway
        .open_browser_workspace(&c.browser, Uuid::new_v4(), f.devices[0].id, None)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = peri_cloud::portal::router_with_gateway(
        c.identity.clone(),
        peri_cloud::portal::PortalConfig::new(&base).unwrap(),
        c.gateway.clone(),
    );
    let stop = CancellationToken::new();
    let shutdown = stop.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .unwrap();
    });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    assert_eq!(
        client
            .get(format!("{base}/api/commands"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let login = client
        .post(format!("{base}/api/login"))
        .header("Origin", &base)
        .json(&json!({"login":"owner","password":PASSWORD}))
        .send()
        .await
        .unwrap();
    assert!(login.status().is_success());
    let cookies: Vec<_> = login
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|h| h.to_str().unwrap().split(';').next().unwrap().to_owned())
        .collect();
    let csrf = cookies
        .iter()
        .find_map(|c| c.strip_prefix("peri_csrf="))
        .unwrap();
    let cookie = cookies.join("; ");
    let catalog = client
        .get(format!("{base}/api/commands"))
        .header("Cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(catalog.status(), 200);
    let url = format!("{base}/api/conversations/{session}/commands");
    let input = json!({"request_id":Uuid::new_v4(),"command":"/new","confirmed":true});
    assert_eq!(
        client
            .post(&url)
            .header("Origin", &base)
            .header("Cookie", &cookie)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let unconfirmed = json!({"request_id":Uuid::new_v4(),"command":"/new","confirmed":false});
    assert_eq!(
        client
            .post(&url)
            .header("Origin", &base)
            .header("Cookie", &cookie)
            .header("x-peri-csrf", csrf)
            .json(&unconfirmed)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    let created = client
        .post(&url)
        .header("Origin", &base)
        .header("Cookie", &cookie)
        .header("x-peri-csrf", csrf)
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 200);
    let created: Value = created.json().await.unwrap();
    let retried: Value = client
        .post(&url)
        .header("Origin", &base)
        .header("Cookie", &cookie)
        .header("x-peri-csrf", csrf)
        .json(&input)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(created["session_id"], retried["session_id"]);
    stop.cancel();
    server.await.unwrap();
    f.finish().await;
}

#[tokio::test]
async fn registered_ui_handler_replaces_unavailable_projection_and_unknown_commands_do_not_infer() {
    let f = Fixture::new(vec![]).await;
    f.pair().await;
    f.select("select", 0).await;
    command(&f, "unknown", "/compcat").await;
    let mut entry = registered("mcp", false);
    entry.fullname = "ui:mcp".into();
    entry.provenance.source = peri_acp_types::command::command_route::CommandSource::Ui;
    f.connector.register_command(entry).unwrap();
    let catalog = f.c().gateway.command_catalog();
    assert_eq!(catalog.iter().filter(|c| c.name == "ui:mcp").count(), 1);
    assert!(catalog
        .iter()
        .any(|c| c.name == "ui:mcp" && c.availability == "available"));
    let task = command(&f, "ui-command", "/ui:mcp").await;
    f.settled(task.session_id.unwrap(), task.turn_id.unwrap())
        .await;
    assert!(f.script.requests.lock().is_empty());
    f.finish().await;
}

#[tokio::test]
async fn native_feedback_channel_is_preserved_without_rewriting_system_prompt() {
    let f = Fixture::new(vec![answer("done")]).await;
    f.pair().await;
    f.select("select", 0).await;
    let mut entry = registered("feedback", false);
    entry.handler = Arc::new(RegisteredCommand {
        wait: false,
        session: true,
    });
    f.connector.register_command(entry).unwrap();
    let command_task = command(&f, "feedback", "/plugin:fixture:feedback").await;
    let session = command_task.session_id.unwrap();
    f.settled(session, command_task.turn_id.unwrap()).await;
    let task = command(&f, "next", "continue").await;
    f.settled(session, task.turn_id.unwrap()).await;
    let messages = f.script.requests.lock()[0]["messages"]
        .as_array()
        .unwrap()
        .clone();
    assert!(messages
        .iter()
        .any(|m| m["role"] == "user" && m.to_string().contains("REGISTERED_COMMAND_DONE")));
    assert!(!messages
        .iter()
        .any(|m| m["role"] == "system" && m.to_string().contains("REGISTERED_COMMAND_DONE")));
    f.finish().await;
}
