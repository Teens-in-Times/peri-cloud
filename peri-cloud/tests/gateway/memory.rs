use super::*;
use peri_cloud::memory::{MemoryContent, MemoryKind, MemorySource, MemoryStore};

fn fact(content: &str) -> MemoryContent {
    MemoryContent {
        content: content.into(),
        kind: MemoryKind::Fact,
        pinned: true,
        valid_until: None,
    }
}

#[tokio::test]
async fn group_commands_and_agent_cannot_disclose_personal_preferences_or_facts() {
    let f = Fixture::new(vec![answer("PRIVATE_MEMORY_MARKER"), answer("群聊回答。")]).await;
    f.pair().await;
    let owner = f.c().browser.principal().id;
    f.c()
        .journal
        .save_preferences(
            owner,
            peri_cloud::state::UserPreferences {
                revision: 0,
                instructions: "PRIVATE_PREFERENCE_MARKER".into(),
            },
        )
        .await
        .unwrap();
    let store = MemoryStore::new(&f.c().journal, owner, 65536);
    store
        .add(
            Uuid::new_v4(),
            0,
            fact("PRIVATE_MEMORY_MARKER"),
            MemorySource::web(),
        )
        .await
        .unwrap();
    f.select("private-memory-device", 0).await;
    let private_turn = f
        .c()
        .gateway
        .receive(f.message("private-turn", "问候"))
        .await
        .unwrap();
    let private_session = private_turn.session_id.unwrap();
    f.settled(private_session, private_turn.turn_id.unwrap())
        .await;
    f.c()
        .gateway
        .receive(f.message("new-private", "/new"))
        .await
        .unwrap();
    f.c().gateway.dispatch().await.unwrap();
    f.adapter.attempts.lock().clear();
    let mut group = f.message("group-select", &format!("/连接 {}", f.devices[0].id));
    group.route.conversation_id = "group:fixture".into();
    f.c().gateway.receive(group.clone()).await.unwrap();
    f.c().gateway.dispatch().await.unwrap();
    group.event_id = "group-resume-private".into();
    group.text = format!("/resume {private_session}");
    f.c().gateway.receive(group.clone()).await.unwrap();
    f.c().gateway.dispatch().await.unwrap();
    assert!(serde_json::to_string(&*f.adapter.attempts.lock())
        .unwrap()
        .contains("空白群会话"));
    group.event_id = "group-history".into();
    group.text = "/threads".into();
    f.c().gateway.receive(group.clone()).await.unwrap();
    f.c().gateway.dispatch().await.unwrap();
    assert!(!serde_json::to_string(&*f.adapter.attempts.lock())
        .unwrap()
        .contains(&private_session.to_string()));
    group.event_id = "group-memory".into();
    group.text = "/记忆".into();
    f.c().gateway.receive(group.clone()).await.unwrap();
    f.c().gateway.dispatch().await.unwrap();
    let deliveries = serde_json::to_string(&*f.adapter.attempts.lock()).unwrap();
    assert!(!deliveries.contains("PRIVATE_MEMORY_MARKER"));
    assert!(!deliveries.contains("PRIVATE_PREFERENCE_MARKER"));
    assert!(deliveries.contains("私聊"));
    group.event_id = "group-turn".into();
    group.text = "下午好".into();
    let receipt = f.c().gateway.receive(group).await.unwrap();
    f.settled(receipt.session_id.unwrap(), receipt.turn_id.unwrap())
        .await;
    let requests = f.script.requests.lock().clone();
    let serialized = serde_json::to_string(&requests[1]).unwrap();
    assert!(!serialized.contains("PRIVATE_MEMORY_MARKER"));
    assert!(!serialized.contains("PRIVATE_PREFERENCE_MARKER"));
    let tools = serde_json::to_string(&requests[1]["tools"]).unwrap();
    for name in [
        "SearchMemory",
        "GetMemory",
        "AddMemory",
        "UpdateMemory",
        "ForgetMemory",
    ] {
        assert!(!tools.contains(name));
    }
    f.finish().await;
}

#[tokio::test]
async fn scheduled_and_normal_turns_read_current_memory_through_the_same_context_entry() {
    use peri_cloud::scheduler::{NewSchedule, Notify, ScheduleMode, Timing};
    let f = Fixture::new_for(vec![answer("下午好呀。"), answer("新的对话。")], true).await;
    let c = f.c();
    let owner = c.browser.principal().id;
    let session = c
        .gateway
        .open_browser_workspace(&c.browser, Uuid::new_v4(), f.devices[0].id, None)
        .await
        .unwrap();
    let store = MemoryStore::new(&c.journal, owner, 65536);
    let item = store
        .add(
            Uuid::new_v4(),
            0,
            fact("OLD_MEMORY_MARKER"),
            MemorySource::web(),
        )
        .await
        .unwrap();
    let schedule = c
        .gateway
        .create_browser_schedule(
            &c.browser,
            NewSchedule {
                request_id: Uuid::new_v4(),
                session_id: session,
                title: "问候".into(),
                prompt: "说下午好".into(),
                timing: Timing::After { seconds: 60 },
                mode: ScheduleMode::Conversation,
                notify: Notify::Always,
            },
        )
        .await
        .unwrap();
    store
        .update(
            item.id,
            1,
            fact("CURRENT_MEMORY_MARKER"),
            MemorySource::web(),
        )
        .await
        .unwrap();
    f.clock
        .0
        .store(schedule.next_fire.unwrap(), Ordering::SeqCst);
    c.gateway.schedule_tick().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            c.gateway.dispatch().await.unwrap();
            let runs = c
                .gateway
                .browser_schedule_runs(&c.browser, schedule.id)
                .await
                .unwrap();
            if runs.first().is_some_and(|r| r.state == "completed") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .unwrap();
    {
        let requests = f.script.requests.lock().clone();
        let serialized = serde_json::to_string(&requests[0]).unwrap();
        assert!(serialized.contains("CURRENT_MEMORY_MARKER"));
        assert!(!serialized.contains("OLD_MEMORY_MARKER"));
        let tools = serde_json::to_string(&requests[0]["tools"]).unwrap();
        assert!(tools.contains("SearchMemory"));
        assert!(tools.contains("GetMemory"));
        assert!(!tools.contains("AddMemory"));
        assert!(!tools.contains("ForgetMemory"));
    }
    store.forget(item.id, 2, MemorySource::web()).await.unwrap();
    let receipt = c
        .gateway
        .submit_browser_task(&c.browser, session, Uuid::new_v4(), "继续聊天".into())
        .await
        .unwrap();
    f.settled(session, receipt.turn_id).await;
    let requests = f.script.requests.lock().clone();
    let latest = &requests[1];
    let system = latest["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "system")
        .map(|m| m.to_string())
        .collect::<String>();
    assert!(!system.contains("CURRENT_MEMORY_MARKER"));
    assert!(
        !latest.to_string().contains("CURRENT_MEMORY_MARKER"),
        "forget must filter the complete model view, including old frames"
    );
    assert!(serde_json::to_string(&latest["tools"])
        .unwrap()
        .contains("AddMemory"));
    f.finish().await;
}

#[tokio::test]
async fn memory_http_enforces_browser_csrf_origin_owner_and_item_versions() {
    use peri_cloud::portal::{router_with_gateway, PortalConfig};
    use reqwest::StatusCode;
    let f = Fixture::new(Vec::new()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = router_with_gateway(
        f.c().identity.clone(),
        PortalConfig::new(&base).unwrap(),
        f.c().gateway.clone(),
    );
    let shutdown = CancellationToken::new();
    let stop = shutdown.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(stop.cancelled_owned())
            .await
            .unwrap();
    });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let login = f
        .c()
        .identity
        .login("owner", PASSWORD.into())
        .await
        .unwrap();
    let cookie = format!("peri_session={}", login.session_token);
    let invite = f.c().identity.create_invite(&f.c().browser).await.unwrap();
    let guest = f
        .c()
        .identity
        .register_invited(&invite.code, "guest-memory", PASSWORD.into(), "Guest")
        .await
        .unwrap();
    let guest_login = f
        .c()
        .identity
        .login("guest-memory", PASSWORD.into())
        .await
        .unwrap();
    let guest_cookie = format!("peri_session={}", guest_login.session_token);
    let store = MemoryStore::new(&f.c().journal, guest.id, 65536);
    let private = store
        .add(
            Uuid::new_v4(),
            0,
            fact("GUEST_PRIVATE_BODY"),
            MemorySource::web(),
        )
        .await
        .unwrap();
    let input = json!({"request_id":Uuid::new_v4(),"generation":0,"value":fact("东京服务器 <img onerror=alert(1)>")});
    assert_eq!(
        client
            .get(format!("{base}/api/memories"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .post(format!("{base}/api/memories"))
            .header("Cookie", &cookie)
            .header("Origin", &base)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .post(format!("{base}/api/memories"))
            .header("Cookie", &cookie)
            .header("Origin", "https://untrusted.example")
            .header("X-Peri-CSRF", &login.csrf_token)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let post = |path: &str, body: Value| {
        client
            .post(format!("{base}{path}"))
            .header("Cookie", &cookie)
            .header("Origin", &base)
            .header("X-Peri-CSRF", &login.csrf_token)
            .json(&body)
    };
    let saved: Value = post("/api/memories", input.clone())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let replay: Value = post("/api/memories", input.clone())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(saved["id"], replay["id"]);
    assert_eq!(saved["source"]["kind"], "web");
    let mut spoof = input;
    spoof["owner"] = json!(guest.id);
    assert_eq!(
        post("/api/memories", spoof).send().await.unwrap().status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        client
            .get(format!("{base}/api/memories/{}", private.id))
            .header("Cookie", &cookie)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let owner_id = saved["id"].as_str().unwrap();
    assert_eq!(
        client
            .get(format!("{base}/api/memories/{owner_id}"))
            .header("Cookie", &guest_cookie)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let foreign_update = post(
        &format!("/api/memories/{}/edit", private.id),
        json!({"revision":1,"value":fact("spoofed")}),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(foreign_update.status(), StatusCode::CONFLICT);
    post(
        &format!("/api/memories/{owner_id}/edit"),
        json!({"revision":1,"value":fact("已更正")}),
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap();
    assert_eq!(
        post(
            &format!("/api/memories/{owner_id}/forget"),
            json!({"revision":1})
        )
        .send()
        .await
        .unwrap()
        .status(),
        StatusCode::CONFLICT
    );
    post(
        &format!("/api/memories/{owner_id}/forget"),
        json!({"revision":2}),
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap();
    let page: Value = client
        .get(format!("{base}/api/memories"))
        .header("Cookie", &cookie)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["page"]["total"], 0);
    assert!(!page.to_string().contains("GUEST_PRIVATE_BODY"));
    let fleet=post("/api/memories",json!({"request_id":Uuid::new_v4(),"generation":page["page"]["generation"],"scope":{"kind":"fleet"},"value":fact("FLEET_SCOPE_BODY")})).send().await.unwrap().error_for_status().unwrap().json::<Value>().await.unwrap();
    assert_eq!(fleet["scope"]["kind"], "fleet");
    let device_id = f.devices[0].id;
    let device=post("/api/memories",json!({"request_id":Uuid::new_v4(),"generation":page["page"]["generation"].as_u64().unwrap()+1,"scope":{"kind":"device_env","device_id":device_id},"value":fact("DEVICE_SCOPE_BODY")})).send().await.unwrap().error_for_status().unwrap().json::<Value>().await.unwrap();
    assert_eq!(device["scope"]["device_id"], device_id.to_string());
    let fleet_page: Value = client
        .get(format!("{base}/api/memories?scope=fleet"))
        .header("Cookie", &cookie)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(fleet_page.to_string().contains("FLEET_SCOPE_BODY"));
    assert!(!fleet_page.to_string().contains("DEVICE_SCOPE_BODY"));
    f.c()
        .identity
        .revoke_device(&f.c().browser, device_id)
        .await
        .unwrap();
    let retained: Value = client
        .get(format!("{base}/api/memories?scope=device_env:{device_id}"))
        .header("Cookie", &cookie)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(retained.to_string().contains("DEVICE_SCOPE_BODY"));
    assert!(retained["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["key"] == format!("device_env:{device_id}") && s["read_only"] == true));
    let edit_path = format!("/api/memories/{}/edit", device["id"].as_str().unwrap());
    assert_eq!(post(&edit_path,json!({"scope":{"kind":"device_env","device_id":device_id},"revision":1,"value":fact("should not update revoked scope")})).send().await.unwrap().status(),StatusCode::FORBIDDEN);
    let forget_path = format!("/api/memories/{}/forget", device["id"].as_str().unwrap());
    post(
        &forget_path,
        json!({"scope":{"kind":"device_env","device_id":device_id},"revision":1}),
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap();
    assert_eq!(
        client
            .get(format!(
                "{base}/api/memories/{}",
                fleet["id"].as_str().unwrap()
            ))
            .header("Cookie", &cookie)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        client
            .get(format!("{base}/api/memories?scope=device_env:{device_id}"))
            .header("Cookie", &guest_cookie)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        client
            .get(format!(
                "{base}/api/memories?scope=context:{}",
                Uuid::new_v4()
            ))
            .header("Cookie", &cookie)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        client
            .get(format!("{base}/api/user-data"))
            .header("Cookie", &cookie)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    shutdown.cancel();
    server.await.unwrap();
    f.finish().await;
}
