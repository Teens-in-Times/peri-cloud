use super::menu_tests::{choice, menu};
use super::*;
use peri_cloud::gateway::access::{AccessChange, AccessList};
use peri_cloud::gateway::MenuCard;

async fn click(f: &Fixture, event: &str, card: &MenuCard, label: &str) -> MenuCard {
    menu(f, event, &format!("/卡片 {}", choice(card, label)))
        .await
        .0
}
async fn controls(f: &Fixture, event: &str) -> MenuCard {
    let (home, _) = menu(f, &format!("{event}-home"), "/网关").await;
    click(f, &format!("{event}-target"), &home, "管理 fixture-qq").await
}
async fn add(f: &Fixture, event: &str, card: &MenuCard, section: &str, id: &str) -> MenuCard {
    let entries = click(f, &format!("{event}-list"), card, section).await;
    f.c()
        .gateway
        .receive(f.message(
            &format!("{event}-prompt"),
            &format!("/卡片 {}", choice(&entries, "添加一项")),
        ))
        .await
        .unwrap();
    f.c().gateway.dispatch().await.unwrap();
    assert!(
        matches!(&f.adapter.attempts.lock().last().unwrap().body,DeliveryBody::Notice{text} if text.contains("不会交给 Agent"))
    );
    menu(f, &format!("{event}-value"), id).await.0
}

#[tokio::test]
async fn access_menus_enforce_user_and_group_intersection_blacklist_priority_and_persisted_revisions(
) {
    let mut f = Fixture::new(Vec::new()).await;
    f.pair().await;
    let current = controls(&f, "access").await;
    let current = add(
        &f,
        "self",
        &current,
        "允许的用户（0）",
        &f.route.identity.external_user_id,
    )
    .await;
    let stale = click(&f, "old-list", &current, "允许的用户（1）").await;
    let mode = click(&f, "modes", &current, "切换名单模式").await;
    let confirm = click(&f, "white", &mode, "使用白名单模式").await;
    let current = click(&f, "white-confirm", &confirm, "确认切换").await;
    let mut stranger = f.message("stranger", "菜单");
    stranger.route.identity.external_user_id = "stranger".into();
    assert!(matches!(
        f.c().gateway.receive(stranger).await,
        Err(GatewayError::Identity(IdentityError::Forbidden))
    ));
    let mut group = f.message("unlisted-group", "菜单");
    group.route.conversation_id = "group:demo-group".into();
    assert!(f.c().gateway.receive(group.clone()).await.is_err());
    let current = add(&f, "group", &current, "允许的群（0）", "demo-group").await;
    group.event_id = "allowed-group".into();
    f.c().gateway.receive(group.clone()).await.unwrap();
    f.c().gateway.dispatch().await.unwrap();
    assert_eq!(f.adapter.attempts.lock().last().unwrap().route, group.route);
    let current = add(&f, "block", &current, "屏蔽的群（0）", "demo-group").await;
    group.event_id = "blocked-group".into();
    assert!(f.c().gateway.receive(group.clone()).await.is_err());
    // A pre-edit action cannot overwrite the newer policy, even without a session switch.
    f.c()
        .gateway
        .receive(f.message(
            "stale-action",
            &format!("/卡片 {}", choice(&stale, "添加一项")),
        ))
        .await
        .unwrap();
    f.c().gateway.dispatch().await.unwrap();
    assert!(
        matches!(&f.adapter.attempts.lock().last().unwrap().body,DeliveryBody::Notice{text} if text.contains("修改"))
    );
    // Prevent losing the current management entry by removing its own user.
    let users = click(&f, "self-removal-list", &current, "允许的用户（1）").await;
    f.c()
        .gateway
        .receive(f.message(
            "self-remove",
            &format!(
                "/卡片 {}",
                choice(
                    &users,
                    &format!("移除 {}", f.route.identity.external_user_id)
                )
            ),
        ))
        .await
        .unwrap();
    f.c().gateway.dispatch().await.unwrap();
    assert!(
        matches!(&f.adapter.attempts.lock().last().unwrap().body,DeliveryBody::Notice{text} if text.contains("关闭当前管理入口"))
    );
    assert!(f.c().gateway.route_allowed(&f.route).await.unwrap());
    f.restart().await;
    assert!(
        !f.c().gateway.route_allowed(&group.route).await.unwrap(),
        "bootstrap must not overwrite edited policy"
    );
    let current = controls(&f, "restart").await;
    assert!(current.text.contains("版本：4"));
    assert!(current.buttons.iter().any(|b| b.label == "屏蔽的群（1）"));
    assert!(
        f.script.requests.lock().is_empty(),
        "access control never invokes a model"
    );
    f.finish().await;
}

#[tokio::test]
async fn access_drafts_are_route_owned_expire_without_model_calls_and_group_admin_is_rejected() {
    let f = Fixture::new(Vec::new()).await;
    f.pair().await;
    let card = controls(&f, "draft").await;
    let list = click(&f, "list", &card, "允许的群（0）").await;
    f.c()
        .gateway
        .receive(f.message("prompt", &format!("/卡片 {}", choice(&list, "添加一项"))))
        .await
        .unwrap();
    // Another chat cannot edit or consume this private draft.
    let mut group = f.message("group-admin", "/网关");
    group.route.conversation_id = "group:demo-group".into();
    f.c().gateway.receive(group).await.unwrap();
    f.c().gateway.dispatch().await.unwrap();
    assert!(
        matches!(&f.adapter.attempts.lock().last().unwrap().body,DeliveryBody::Notice{text} if text.contains("管理员")&&text.contains("私聊"))
    );
    f.clock.0.fetch_add(241, Ordering::SeqCst);
    f.c()
        .gateway
        .receive(f.message("expired-value", "demo-group"))
        .await
        .unwrap();
    f.c().gateway.dispatch().await.unwrap();
    assert!(
        matches!(&f.adapter.attempts.lock().last().unwrap().body,DeliveryBody::Notice{text} if text.contains("过期")&&text.contains("没有调用 Agent"))
    );
    assert!(f.script.requests.lock().is_empty());
    f.finish().await;
}

#[tokio::test]
async fn access_web_recovery_requires_admin_csrf_and_suppresses_previously_queued_group_delivery() {
    let mut f = Fixture::new(Vec::new()).await;
    f.pair().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = peri_cloud::portal::router_with_gateway(
        f.c().identity.clone(),
        peri_cloud::portal::PortalConfig::new(&origin).unwrap(),
        f.c().gateway.clone(),
    );
    let stop = f.stop.clone();
    f.servers.push(tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(stop.cancelled_owned())
            .await
            .unwrap();
    }));
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = format!("{origin}/api/admin/gateway-access");
    assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
    let owner = f
        .c()
        .identity
        .login("owner", PASSWORD.into())
        .await
        .unwrap();
    let cookie = format!(
        "peri_session={}; peri_csrf={}",
        owner.session_token, owner.csrf_token
    );
    let invite = f.c().identity.create_invite(&f.c().browser).await.unwrap();
    f.c()
        .identity
        .register_invited(&invite.code, "guest", PASSWORD.into(), "Guest")
        .await
        .unwrap();
    let guest = f
        .c()
        .identity
        .login("guest", PASSWORD.into())
        .await
        .unwrap();
    let guest_cookie = format!(
        "peri_session={}; peri_csrf={}",
        guest.session_token, guest.csrf_token
    );
    assert_eq!(
        client
            .get(&url)
            .header("Cookie", guest_cookie)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let data: Value = client
        .get(&url)
        .header("Cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(data[0]["record"]["revision"], 0);
    let body = json!({"adapter":"fixture-qq","revision":0,"change":{"kind":"add","list":"blocked_groups","id":"demo-group"}});
    assert_eq!(
        client
            .post(&url)
            .header("Origin", &origin)
            .header("Cookie", &cookie)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        client
            .post(&url)
            .header("Origin", "https://untrusted.example")
            .header("Cookie", &cookie)
            .header("X-Peri-CSRF", &owner.csrf_token)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    f.adapter.attempts.lock().clear();
    let mut group = f.message("queued-group", "菜单");
    group.route.conversation_id = "group:demo-group".into();
    f.c().gateway.receive(group.clone()).await.unwrap();
    assert_eq!(
        client
            .post(&url)
            .header("Origin", &origin)
            .header("Cookie", &cookie)
            .header("X-Peri-CSRF", &owner.csrf_token)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    f.c().gateway.dispatch().await.unwrap();
    assert!(
        f.adapter.attempts.lock().is_empty(),
        "queued reply to newly blocked group must be suppressed"
    );
    assert_eq!(
        client
            .post(&url)
            .header("Origin", &origin)
            .header("Cookie", &cookie)
            .header("X-Peri-CSRF", &owner.csrf_token)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    assert!(f.c().gateway.receive(group.clone()).await.is_err());
    // Web recovery can remove a block independently of any chat source.
    f.c()
        .gateway
        .change_browser_access(
            &f.c().browser,
            "fixture-qq",
            1,
            &AccessChange::Remove {
                list: AccessList::BlockedGroups,
                id: "demo-group".into(),
            },
        )
        .await
        .unwrap();
    assert!(f.c().gateway.route_allowed(&group.route).await.unwrap());
    let audit = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new()
            .filename(f.root.path().join("cloud.sqlite"))
            .read_only(true),
    )
    .await
    .unwrap();
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM gateway_access_audit")
        .fetch_one(&audit)
        .await
        .unwrap();
    assert_eq!(count, 2);
    audit.close().await;
    assert!(f.script.requests.lock().is_empty());
    f.finish().await;
}
