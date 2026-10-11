use super::*;
use peri_cloud::gateway::MenuCard;

async fn menu(f: &Fixture, event: &str, command: &str) -> (MenuCard, String) {
    f.c()
        .gateway
        .receive(f.message(event, command))
        .await
        .unwrap();
    f.c().gateway.dispatch().await.unwrap();
    let delivery = f.adapter.attempts.lock().last().unwrap().clone();
    let DeliveryBody::Menu { card } = delivery.body else {
        panic!("expected menu");
    };
    (card, delivery.in_reply_to)
}

fn choice(card: &MenuCard, label: &str) -> Uuid {
    card.buttons
        .iter()
        .find(|b| b.label == label)
        .unwrap_or_else(|| panic!("missing {label} in {card:?}"))
        .action_id
}

#[tokio::test]
async fn menus_discover_new_directories_set_defaults_and_keep_frozen_sessions() {
    let mut f = Fixture::new(Vec::new()).await;
    f.pair().await;
    let (instances, _) = menu(&f, "instances", "实例").await;
    assert_eq!(instances.buttons.len(), 3);
    menu(
        &f,
        "select",
        &format!(
            "/卡片 {}",
            instances
                .buttons
                .iter()
                .find(|b| b.label.ends_with("PC 0"))
                .unwrap()
                .action_id
        ),
    )
    .await;
    let old = f
        .c()
        .gateway
        .browser_workspaces(&f.c().browser)
        .await
        .unwrap()
        .remove(0);
    let project = f.workspaces[0].path().join("new project");
    std::fs::create_dir(&project).unwrap();
    let (workspaces, _) = menu(&f, "workspaces", "工作区").await;
    let (browser, origin) = menu(
        &f,
        "browse",
        &format!("/卡片 {}", choice(&workspaces, "浏览当前目录")),
    )
    .await;
    assert_eq!(
        origin, "browse",
        "each new message has its own passive reply budget"
    );
    let (folder, _) = menu(
        &f,
        "folder",
        &format!("/卡片 {}", choice(&browser, "new project")),
    )
    .await;
    let (saved, _) = menu(
        &f,
        "save-default",
        &format!("/卡片 {}", choice(&folder, "设为全局默认")),
    )
    .await;
    let current = f
        .c()
        .gateway
        .browser_workspaces(&f.c().browser)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(current.session_id, old.session_id);
    assert_eq!(current.workspace, old.workspace);
    assert_eq!(
        current.default_workspace,
        project.canonicalize().unwrap().to_str().unwrap()
    );
    menu(
        &f,
        "use-default",
        &format!("/卡片 {}", choice(&saved, "使用默认工作区")),
    )
    .await;
    let updated = f
        .c()
        .gateway
        .browser_workspaces(&f.c().browser)
        .await
        .unwrap()
        .remove(0);
    assert_ne!(updated.session_id, old.session_id);
    assert_eq!(updated.workspace, current.default_workspace);
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
    f.restart().await;
    assert_eq!(
        f.c()
            .identity
            .devices(&f.c().browser)
            .await
            .unwrap()
            .iter()
            .find(|d| d.id == f.devices[0].id)
            .unwrap()
            .default_workspace,
        current.default_workspace
    );
    f.finish().await;
}

#[tokio::test]
async fn menu_actions_are_bound_to_sender_route_expiry_and_current_session() {
    let f = Fixture::new(Vec::new()).await;
    f.pair().await;
    let (instances, _) = menu(&f, "instances", "实例").await;
    let token = instances.buttons[0].action_id;
    let mut wrong = f.route.clone();
    wrong.conversation_id = "different-group".into();
    assert!(matches!(
        f.c()
            .gateway
            .receive_menu(wrong, "wrong-group".into(), token)
            .await,
        Err(GatewayError::StaleInteraction)
    ));
    let mut wrong = f.route.clone();
    wrong.identity.external_user_id = "stranger".into();
    assert!(matches!(
        f.c()
            .gateway
            .receive_menu(wrong, "wrong-user".into(), token)
            .await,
        Err(GatewayError::Identity(_))
    ));
    menu(&f, "select", &format!("/卡片 {token}")).await;
    let selected = f
        .c()
        .gateway
        .browser_workspaces(&f.c().browser)
        .await
        .unwrap()
        .remove(0);
    f.c()
        .gateway
        .receive(f.message(
            "stale",
            &format!("/卡片 {}", instances.buttons[1].action_id),
        ))
        .await
        .unwrap();
    assert_eq!(
        f.c()
            .gateway
            .browser_workspaces(&f.c().browser)
            .await
            .unwrap()[0]
            .session_id,
        selected.session_id
    );
    let (settings, _) = menu(&f, "permissions", "/权限").await;
    let permission = choice(&settings, "全部");
    f.clock.0.fetch_add(241, Ordering::SeqCst);
    assert!(matches!(
        f.c()
            .gateway
            .receive_menu(f.route.clone(), "expired".into(), permission)
            .await,
        Err(GatewayError::StaleInteraction)
    ));
    assert_eq!(
        f.c()
            .journal
            .session(f.c().browser.principal().id, selected.session_id)
            .await
            .unwrap()
            .permissions()
            .unwrap(),
        peri_acp_types::permission::PermissionMode::Default
    );
    f.finish().await;
}

#[tokio::test]
async fn numbered_fallback_deduplicates_and_invalid_numbers_complete_cleanly() {
    let f = Fixture::new(Vec::new()).await;
    f.pair().await;
    menu(&f, "instances", "实例").await;
    let message = f.message("number-choice", "/选 1");
    f.c().gateway.receive(message.clone()).await.unwrap();
    f.c().gateway.dispatch().await.unwrap();
    let selected = f
        .c()
        .gateway
        .browser_workspaces(&f.c().browser)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(
        f.adapter.attempts.lock().last().unwrap().in_reply_to,
        "number-choice"
    );
    assert_eq!(
        f.c().gateway.receive(message).await.unwrap().state,
        ReceiptState::Completed
    );
    assert_eq!(
        f.c()
            .gateway
            .browser_workspaces(&f.c().browser)
            .await
            .unwrap()[0]
            .session_id,
        selected.session_id
    );
    for (id, text) in [("invalid", "/选 wrong"), ("out-of-range", "/选 99")] {
        assert_eq!(
            f.c()
                .gateway
                .receive(f.message(id, text))
                .await
                .unwrap()
                .state,
            ReceiptState::Completed
        );
    }
    f.finish().await;
}

#[tokio::test]
async fn native_menu_queue_is_owned_and_revoked_instances_cannot_be_selected() {
    let f = Fixture::new(Vec::new()).await;
    f.pair().await;
    let (instances, _) = menu(&f, "instances", "实例").await;
    f.c()
        .identity
        .revoke_device(&f.c().browser, f.devices[0].id)
        .await
        .unwrap();
    f.c()
        .gateway
        .receive_menu(
            f.route.clone(),
            "revoked".into(),
            instances.buttons[0].action_id,
        )
        .await
        .unwrap();
    assert!(f
        .c()
        .gateway
        .shutdown(Duration::from_secs(3))
        .await
        .unwrap());
    assert!(f
        .c()
        .gateway
        .browser_workspaces(&f.c().browser)
        .await
        .unwrap()
        .is_empty());
    assert!(f.script.requests.lock().is_empty());
    f.finish().await;
}

#[tokio::test]
async fn browser_discovery_requires_owner_and_defaults_survive_native_metadata_refresh() {
    let mut f = Fixture::new(Vec::new()).await;
    f.pair().await;
    let path = f.workspaces[0].path().join("unseen");
    std::fs::create_dir(&path).unwrap();
    let listing = f
        .c()
        .gateway
        .browse_browser_instance(&f.c().browser, f.devices[0].id, None, 0)
        .await
        .unwrap();
    assert_eq!(listing.directories[0].name, "unseen");
    assert!(matches!(
        f.c()
            .gateway
            .browse_browser_instance(&f.c().browser, Uuid::new_v4(), None, 0)
            .await,
        Err(GatewayError::Identity(IdentityError::Forbidden))
    ));
    assert!(matches!(
        f.c()
            .gateway
            .set_browser_instance_default(&f.c().browser, f.devices[0].id, "".into())
            .await,
        Err(GatewayError::Invalid)
    ));
    let saved = f
        .c()
        .gateway
        .set_browser_instance_default(
            &f.c().browser,
            f.devices[0].id,
            path.to_str().unwrap().into(),
        )
        .await
        .unwrap();
    f.restart().await;
    f.select("default-select", 0).await;
    assert_eq!(
        f.c()
            .gateway
            .browser_workspaces(&f.c().browser)
            .await
            .unwrap()[0]
            .workspace,
        saved
    );
    f.c()
        .identity
        .revoke_device(&f.c().browser, f.devices[0].id)
        .await
        .unwrap();
    assert!(f
        .c()
        .gateway
        .browse_browser_instance(&f.c().browser, f.devices[0].id, None, 0)
        .await
        .is_err());
    f.finish().await;
}
