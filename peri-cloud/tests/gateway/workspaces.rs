use super::*;

#[tokio::test]
async fn workspace_switch_preserves_permissions_freezes_history_and_survives_restart() {
    let mut f = Fixture::new(vec![tool_write(), answer("已写入指定目录。")]).await;
    f.pair().await;
    f.select("default-workspace", 0).await;
    f.c()
        .gateway
        .receive(f.message("bypass", "/权限 全部"))
        .await
        .unwrap();
    let old = f
        .c()
        .gateway
        .browser_workspaces(&f.c().browser)
        .await
        .unwrap()
        .remove(0);
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("project with spaces");
    std::fs::create_dir(&path).unwrap();
    let path = path.to_str().unwrap();
    f.c()
        .gateway
        .receive(f.message("new-workspace", &format!("/工作区 {path}")))
        .await
        .unwrap();
    let selected = f
        .c()
        .gateway
        .browser_workspaces(&f.c().browser)
        .await
        .unwrap()
        .remove(0);
    assert_ne!(old.session_id, selected.session_id);
    assert_eq!(
        selected.workspace,
        std::fs::canonicalize(path).unwrap().to_str().unwrap()
    );
    assert_eq!(
        f.c()
            .journal
            .session(f.c().browser.principal().id, selected.session_id)
            .await
            .unwrap()
            .permissions()
            .unwrap(),
        peri_acp_types::permission::PermissionMode::Bypass
    );
    assert_eq!(
        f.c()
            .journal
            .session(f.c().browser.principal().id, old.session_id)
            .await
            .unwrap()
            .frozen
            .binding
            .workspace,
        old.workspace
    );
    assert!(matches!(
        f.c()
            .gateway
            .switch_browser_workspace(&f.c().browser, old.session_id, old.workspace)
            .await,
        Err(GatewayError::StaleInteraction)
    ));
    f.restart().await;
    let restored = f
        .c()
        .gateway
        .browser_workspaces(&f.c().browser)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(restored.session_id, selected.session_id);
    assert_eq!(restored.workspace, selected.workspace);
    let receipt = f
        .c()
        .gateway
        .receive(f.message("write-selected", "写入当前工作区"))
        .await
        .unwrap();
    f.settled(receipt.session_id.unwrap(), receipt.turn_id.unwrap())
        .await;
    assert_eq!(
        std::fs::read_to_string(std::path::Path::new(path).join("agent.txt")).unwrap(),
        "PRIVATE_TOOL_CONTENT"
    );
    assert!(!f.workspaces[0].path().join("agent.txt").exists());
    let before = f
        .c()
        .journal
        .session(f.c().browser.principal().id, selected.session_id)
        .await
        .unwrap();
    assert!(!before.history.is_empty());
    let next = f
        .c()
        .gateway
        .switch_browser_workspace(
            &f.c().browser,
            selected.session_id,
            f.devices[0].default_workspace.clone(),
        )
        .await
        .unwrap();
    assert_ne!(next, selected.session_id);
    let history = f
        .c()
        .journal
        .session(f.c().browser.principal().id, selected.session_id)
        .await
        .unwrap();
    assert_eq!(history.history.len(), before.history.len());
    f.finish().await;
}

#[tokio::test]
async fn workspace_invalid_busy_and_unlinked_requests_cannot_change_selection() {
    let f = Fixture::new(vec![tool_write(), answer("操作未获批准。")]).await;
    f.pair().await;
    f.select("select", 0).await;
    let selected = f
        .c()
        .gateway
        .browser_workspaces(&f.c().browser)
        .await
        .unwrap()
        .remove(0);
    assert!(matches!(
        f.c()
            .gateway
            .switch_browser_workspace(&f.c().browser, selected.session_id, "relative/path".into())
            .await,
        Err(GatewayError::Invalid)
    ));
    let missing = f.workspaces[0]
        .path()
        .join("missing")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(matches!(
        f.c()
            .gateway
            .switch_browser_workspace(&f.c().browser, selected.session_id, missing)
            .await,
        Err(GatewayError::Connection)
    ));
    f.adapter.attempts.lock().clear();
    f.c()
        .gateway
        .receive(f.message("pending-write", "写文件"))
        .await
        .unwrap();
    let card = f.card().await;
    assert!(matches!(
        f.c()
            .gateway
            .switch_browser_workspace(
                &f.c().browser,
                selected.session_id,
                f.devices[1].default_workspace.clone()
            )
            .await,
        Err(GatewayError::Busy)
    ));
    let busy = f
        .c()
        .gateway
        .browser_workspaces(&f.c().browser)
        .await
        .unwrap()
        .remove(0);
    assert!(busy.busy);
    assert_eq!(busy.session_id, selected.session_id);
    f.c()
        .gateway
        .respond(&f.route, card.request_id, InteractionAction::Reject {})
        .await
        .unwrap();
    f.settled(card.session_id, card.turn_id).await;
    f.c()
        .identity
        .revoke_channel(&f.c().browser, &f.route.identity)
        .await
        .unwrap();
    assert!(f
        .c()
        .gateway
        .browser_workspaces(&f.c().browser)
        .await
        .unwrap()
        .is_empty());
    assert!(f
        .c()
        .gateway
        .switch_browser_workspace(
            &f.c().browser,
            selected.session_id,
            f.devices[1].default_workspace.clone()
        )
        .await
        .is_err());
    f.finish().await;
}

#[tokio::test]
async fn workspace_portal_requires_login_csrf_and_changes_exact_chat_selection() {
    let mut f = Fixture::new(vec![]).await;
    f.pair().await;
    let path = f.workspaces[1].path().to_str().unwrap();
    f.c()
        .gateway
        .receive(f.message("initial-path", &format!("/连接 {} {path}", f.devices[0].id)))
        .await
        .unwrap();
    let selected = f
        .c()
        .gateway
        .browser_workspaces(&f.c().browser)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(
        selected.workspace,
        std::fs::canonicalize(path).unwrap().to_str().unwrap()
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = crate_portal(f.c(), &origin);
    let shutdown = f.stop.clone();
    f.servers.push(tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .unwrap();
    }));
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    assert_eq!(
        client
            .get(format!("{origin}/api/workspaces"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    let login = f
        .c()
        .identity
        .login("owner", PASSWORD.into())
        .await
        .unwrap();
    let cookies = format!(
        "peri_session={}; peri_csrf={}",
        login.session_token, login.csrf_token
    );
    let url = format!("{origin}/api/workspaces/{}/switch", selected.session_id);
    let body = json!({"workspace": f.devices[0].default_workspace});
    assert_eq!(
        client
            .post(&url)
            .header("Origin", &origin)
            .header("Cookie", &cookies)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .post(&url)
            .header("Origin", &origin)
            .header("Cookie", &cookies)
            .header("X-Peri-CSRF", &login.csrf_token)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(
        client
            .post(&url)
            .header("Origin", &origin)
            .header("Cookie", &cookies)
            .header("X-Peri-CSRF", &login.csrf_token)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::CONFLICT
    );
    let list: Value = client
        .get(format!("{origin}/api/workspaces"))
        .header("Cookie", cookies)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        list[0]["workspace"],
        std::fs::canonicalize(&f.devices[0].default_workspace)
            .unwrap()
            .to_str()
            .unwrap()
    );
    assert_eq!(list[0]["known_workspaces"].as_array().unwrap().len(), 2);
    f.finish().await;
}

fn crate_portal(cloud: &CloudSide, origin: &str) -> Router {
    peri_cloud::portal::router_with_gateway(
        cloud.identity.clone(),
        peri_cloud::portal::PortalConfig::new(origin).unwrap(),
        cloud.gateway.clone(),
    )
}
