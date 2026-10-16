use super::*;

fn question(multi: bool) -> String {
    let delta = json!({"choices":[{"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"fixture-question","type":"function","function":{"name":"AskUserQuestion","arguments":json!({"questions":[{"question":"要检查哪个服务？","header":"服务","options":[{"label":"A","description":"资源情况"},{"label":"B","description":"网络流量"}],"multiSelect":multi}]}).to_string()}}]},"finish_reason":"tool_calls"}]});
    format!("data: {delta}\n\ndata: [DONE]\n\n")
}

fn last_notice(f: &Fixture) -> String {
    f.adapter
        .attempts
        .lock()
        .iter()
        .rev()
        .find_map(|d| match &d.body {
            DeliveryBody::Notice { text } => Some(text.clone()),
            _ => None,
        })
        .expect("expected a notice")
}

#[tokio::test]
async fn question_button_and_portal_answers_resume_the_real_loop_once() {
    let f = Fixture::new(vec![question(false), answer("开始检查 B。")]).await;
    f.pair().await;
    f.select("select-question", 0).await;
    f.adapter.attempts.lock().clear();
    let receipt = f
        .c()
        .gateway
        .receive(f.message("ask-question", "检查服务"))
        .await
        .unwrap();
    let card = f.card().await;
    assert!(matches!(
        card.context,
        peri_acp_types::interaction::InteractionContext::Questions { .. }
    ));
    let activity = f
        .c()
        .gateway
        .browser_activity(&f.c().browser)
        .await
        .unwrap();
    assert_eq!(activity[0].label, "等待你回答问题");
    f.c()
        .gateway
        .receive(f.message("question-status", "/status"))
        .await
        .unwrap();
    f.c().gateway.dispatch().await.unwrap();
    assert!(last_notice(&f).contains("等待你回答问题"));
    assert!(f
        .c()
        .gateway
        .respond(
            &f.route,
            card.request_id,
            InteractionAction::Choose { option: 99 }
        )
        .await
        .is_err());
    assert!(f
        .c()
        .gateway
        .respond(&f.route, card.request_id, InteractionAction::AllowOnce {})
        .await
        .is_err());
    let mut intruder = f.route.clone();
    intruder.identity.external_user_id = "intruder".into();
    assert!(f
        .c()
        .gateway
        .respond(
            &intruder,
            card.request_id,
            InteractionAction::Choose { option: 1 }
        )
        .await
        .is_err());
    f.c()
        .gateway
        .respond(
            &f.route,
            card.request_id,
            InteractionAction::Choose { option: 1 },
        )
        .await
        .unwrap();
    assert!(f
        .c()
        .gateway
        .respond(
            &f.route,
            card.request_id,
            InteractionAction::Choose { option: 0 }
        )
        .await
        .is_err());
    f.settled(receipt.session_id.unwrap(), receipt.turn_id.unwrap())
        .await;
    assert_eq!(f.script.requests.lock().len(), 2);
    let second = f.script.requests.lock()[1].to_string();
    assert!(second.contains('B'));
    f.finish().await;
}

#[tokio::test]
async fn multi_select_uses_the_same_authenticated_portal_broker() {
    let f = Fixture::new(vec![question(true), answer("已收到你的选择。")]).await;
    f.pair().await;
    f.select("select-multiple", 0).await;
    f.adapter.attempts.lock().clear();
    let receipt = f
        .c()
        .gateway
        .receive(f.message("ask-multiple", "检查两个服务"))
        .await
        .unwrap();
    let card = f.card().await;
    let peri_acp_types::interaction::InteractionContext::Questions { requests } = &card.context
    else {
        panic!("expected question")
    };
    assert!(requests[0].multi_select);
    assert!(f
        .c()
        .gateway
        .respond_browser(
            &f.c().browser,
            card.request_id,
            InteractionAction::Choose { option: 0 }
        )
        .await
        .is_err());
    use peri_acp_types::interaction::QuestionAnswer;
    let action = InteractionAction::Answers {
        answers: vec![QuestionAnswer {
            id: requests[0].id.clone(),
            selected: vec!["A".into(), "B".into()],
            text: Some("先看 B".into()),
        }],
    };
    f.c()
        .gateway
        .respond_browser(&f.c().browser, card.request_id, action)
        .await
        .unwrap();
    f.settled(receipt.session_id.unwrap(), receipt.turn_id.unwrap())
        .await;
    assert_eq!(f.script.requests.lock().len(), 2);
    f.finish().await;
}

#[tokio::test]
async fn unknown_delivery_and_restart_keep_full_results_without_another_model_request() {
    let full = "中文结果🦀\n".repeat(800);
    let mut f = Fixture::new(vec![answer(&full), answer("下一项任务的结果")]).await;
    f.pair().await;
    f.select("result-select", 0).await;
    f.adapter.attempts.lock().clear();
    let receipt = f
        .c()
        .gateway
        .receive(f.message("long-answer", "检查资源情况"))
        .await
        .unwrap();
    f.settled(receipt.session_id.unwrap(), receipt.turn_id.unwrap())
        .await;
    f.adapter.fail_next.store(2, Ordering::SeqCst);
    f.c().gateway.dispatch().await.unwrap();
    let delivery = f.adapter.attempts.lock()[0].clone();
    let DeliveryBody::Reply { reply } = delivery.body else {
        panic!("expected reply")
    };
    assert!(reply.text.chars().count() < 1400);
    assert!(reply
        .text
        .contains(&format!("/结果 {} 1", receipt.turn_id.unwrap())));
    f.restart().await;
    let current = f
        .c()
        .gateway
        .browser_activity(&f.c().browser)
        .await
        .unwrap()[0]
        .session_id;
    let first = f
        .c()
        .gateway
        .browser_result(&f.c().browser, current, 1, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.turn_id, receipt.turn_id.unwrap());
    let mut combined = first.text;
    for page in 2..=first.page_count {
        combined.push_str(
            &f.c()
                .gateway
                .browser_result(&f.c().browser, current, page, Some(first.turn_id))
                .await
                .unwrap()
                .unwrap()
                .text,
        );
    }
    assert_eq!(combined, full);
    assert!(f
        .c()
        .gateway
        .browser_result(&f.c().browser, Uuid::new_v4(), 1, None)
        .await
        .is_err());
    assert!(f
        .c()
        .gateway
        .browser_result(&f.c().browser, current, usize::MAX, None)
        .await
        .is_err());
    assert!(f
        .c()
        .gateway
        .browser_result(&f.c().browser, current, 1, Some(Uuid::new_v4()))
        .await
        .unwrap()
        .is_none());
    f.c()
        .gateway
        .receive(f.message("retrieve-result", "/结果"))
        .await
        .unwrap();
    f.c().gateway.dispatch().await.unwrap();
    assert!(last_notice(&f).contains("已保存结果"));
    assert_eq!(
        f.script.requests.lock().len(),
        1,
        "viewing output must not call the model"
    );
    let second = f
        .c()
        .gateway
        .receive(f.message("another-task", "再检查一次"))
        .await
        .unwrap();
    f.settled(second.session_id.unwrap(), second.turn_id.unwrap())
        .await;
    let pinned = f
        .c()
        .gateway
        .browser_result(&f.c().browser, current, 2, Some(first.turn_id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        pinned.turn_id, first.turn_id,
        "pagination must never mix another turn's result"
    );
    f.finish().await;
}

#[tokio::test]
async fn portal_result_endpoints_require_the_owned_browser_session() {
    let f = Fixture::new(vec![answer("stored-result")]).await;
    f.pair().await;
    f.select("api-select", 0).await;
    let receipt = f
        .c()
        .gateway
        .receive(f.message("api-task", "hello"))
        .await
        .unwrap();
    f.settled(receipt.session_id.unwrap(), receipt.turn_id.unwrap())
        .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = peri_cloud::portal::router_with_gateway(
        f.c().identity.clone(),
        peri_cloud::portal::PortalConfig::new(&format!("http://{address}")).unwrap(),
        f.c().gateway.clone(),
    );
    let stop = CancellationToken::new();
    let signal = stop.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(signal.cancelled_owned())
            .await
            .unwrap();
    });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = format!("http://{address}/api/conversations");
    assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
    let login = f
        .c()
        .identity
        .login("owner", PASSWORD.into())
        .await
        .unwrap();
    let cookie = format!("peri_session={}", login.session_token);
    let response = client
        .get(&url)
        .header("Cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let activity: Value = response.json().await.unwrap();
    assert_eq!(activity[0]["phase"], "idle");
    let result_url = format!("{url}/{}/result", receipt.session_id.unwrap());
    let response = client
        .get(&result_url)
        .header("Cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<Value>().await.unwrap()["text"],
        "stored-result"
    );
    assert_eq!(
        client
            .get(format!("{url}/{}/result", Uuid::new_v4()))
            .header("Cookie", &cookie)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    stop.cancel();
    server.await.unwrap();
    f.finish().await;
}

async fn qq_menu(f: &Fixture, event: &str, command: &str) -> peri_cloud::gateway::MenuCard {
    f.c()
        .gateway
        .receive(f.message(event, command))
        .await
        .unwrap();
    f.c().gateway.dispatch().await.unwrap();
    let delivery = f
        .adapter
        .attempts
        .lock()
        .iter()
        .rev()
        .find(|d| d.in_reply_to == event)
        .expect("expected a command response")
        .clone();
    let DeliveryBody::Menu { card } = delivery.body else {
        panic!("expected QQ menu: {delivery:?}")
    };
    card
}
fn button(menu: &peri_cloud::gateway::MenuCard, label: &str) -> Uuid {
    menu.buttons
        .iter()
        .find(|b| b.label == label)
        .unwrap_or_else(|| panic!("missing {label}: {menu:?}"))
        .action_id
}
fn question_batch(questions: Value) -> String {
    let delta = json!({"choices":[{"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"fixture-question","type":"function","function":{"name":"AskUserQuestion","arguments":json!({"questions":questions}).to_string()}}]},"finish_reason":"tool_calls"}]});
    format!("data: {delta}\n\ndata: [DONE]\n\n")
}

#[tokio::test]
async fn qq_multi_select_pagination_and_text_complete_every_question_without_portal() {
    let options: Vec<_> = (1..=8)
        .map(|i| json!({"label":format!("服务{i}"),"description":format!("第{i}项")}))
        .collect();
    let f=Fixture::new(vec![question_batch(json!([
        {"question":"选择服务","header":"服务","options":options,"multiSelect":true},
        {"question":"如何执行？","header":"方式","options":[{"label":"默认"},{"label":"自定义"}]}
    ])), answer("收到两道题的回答。")]).await;
    f.pair().await;
    f.select("qq-select", 0).await;
    f.adapter.attempts.lock().clear();
    let receipt = f
        .c()
        .gateway
        .receive(f.message("qq-ask", "帮我检查"))
        .await
        .unwrap();
    let card = f.card().await;
    let first = qq_menu(&f, "qq-reopen", "/问题").await;
    assert!(first.title.contains("1/2"));
    let stale_b = button(&first, "服务2");
    let a = button(&first, "服务1");
    let selected = qq_menu(&f, "qq-select-a", &format!("/卡片 {a}")).await;
    assert!(selected.text.contains("✓ 服务1"));
    f.c()
        .gateway
        .receive(f.message("old-card", &format!("/卡片 {stale_b}")))
        .await
        .unwrap();
    f.c().gateway.dispatch().await.unwrap();
    assert!(last_notice(&f).contains("过期"));
    let current = qq_menu(&f, "check-draft", "/问题").await;
    assert!(!current.text.contains("✓ 服务2"));
    let next = qq_menu(
        &f,
        "qq-next-options",
        &format!("/卡片 {}", button(&current, "下一页选项")),
    )
    .await;
    assert!(next.text.contains("答案 7 · 服务7"));
    assert!(next.buttons.len() <= 10);
    let selected = qq_menu(
        &f,
        "qq-select-h",
        &format!("/卡片 {}", button(&next, "服务8")),
    )
    .await;
    let second = qq_menu(
        &f,
        "qq-submit-first",
        &format!("/卡片 {}", button(&selected, "提交本题")),
    )
    .await;
    assert!(second.title.contains("2/2"));
    assert_eq!(
        f.script.requests.lock().len(),
        1,
        "do not resume until every question is answered"
    );
    let code = &card.request_id.simple().to_string()[..8];
    f.c()
        .gateway
        .receive(f.message("qq-question-detail", &format!("/详情 {code}")))
        .await
        .unwrap();
    f.c().gateway.dispatch().await.unwrap();
    assert!(last_notice(&f).contains("服务8"));
    assert_eq!(
        f.script.requests.lock().len(),
        1,
        "details never submit an answer"
    );
    let done = qq_menu(&f, "qq-answer-second", "先检查生产环境，只读查看").await;
    assert_eq!(done.title, "回答已提交");
    f.settled(receipt.session_id.unwrap(), receipt.turn_id.unwrap())
        .await;
    let request = f.script.requests.lock()[1].to_string();
    assert!(request.contains("服务1, 服务8"));
    assert!(!request.contains("服务2,"));
    assert!(request.contains("先检查生产环境，只读查看"));
    assert_eq!(f.script.requests.lock().len(), 2);
    f.finish().await;
}

#[tokio::test]
async fn qq_native_question_callback_is_route_owned_and_duplicate_safe() {
    let f = Fixture::new(vec![question(false), answer("B 完成")]).await;
    f.pair().await;
    f.select("native-select", 0).await;
    f.adapter.attempts.lock().clear();
    let receipt = f
        .c()
        .gateway
        .receive(f.message("native-ask", "检查"))
        .await
        .unwrap();
    f.card().await;
    let menu = qq_menu(&f, "native-reopen", "/问题").await;
    let action = button(&menu, "B");
    let mut stranger = f.route.clone();
    stranger.identity.external_user_id = "another-member".into();
    assert!(f
        .c()
        .gateway
        .receive_menu(stranger, "stranger-click".into(), action)
        .await
        .is_err());
    let mut other_group = f.route.clone();
    other_group.conversation_id = "other-group".into();
    assert!(f
        .c()
        .gateway
        .receive_menu(other_group, "wrong-group-click".into(), action)
        .await
        .is_err());
    for _ in 0..2 {
        f.c()
            .gateway
            .receive_menu(f.route.clone(), "native-button-event".into(), action)
            .await
            .unwrap();
    }
    f.settled(receipt.session_id.unwrap(), receipt.turn_id.unwrap())
        .await;
    assert_eq!(f.script.requests.lock().len(), 2);
    assert!(f.script.requests.lock()[1].to_string().contains("回答: B"));
    assert!(f
        .c()
        .gateway
        .browser_interactions(&f.c().browser)
        .await
        .unwrap()
        .is_empty());
    f.finish().await;
}

#[tokio::test]
async fn qq_text_fallback_submits_multiple_options_without_native_buttons() {
    let f = Fixture::new(vec![question(true), answer("两个服务都已选择")]).await;
    f.pair().await;
    f.select("text-select", 0).await;
    f.adapter.attempts.lock().clear();
    let receipt = f
        .c()
        .gateway
        .receive(f.message("text-ask", "检查"))
        .await
        .unwrap();
    f.card().await;
    let done = qq_menu(&f, "text-answer", "/回答 1，2").await;
    assert_eq!(done.title, "回答已提交");
    f.settled(receipt.session_id.unwrap(), receipt.turn_id.unwrap())
        .await;
    assert!(f.script.requests.lock()[1].to_string().contains("A, B"));
    f.finish().await;
}

#[tokio::test]
async fn qq_approval_detail_is_read_only_and_text_allow_is_exactly_once() {
    let f = Fixture::new(vec![tool_write(), answer("创建完成")]).await;
    f.pair().await;
    f.select("approval-select", 0).await;
    f.adapter.attempts.lock().clear();
    let receipt = f
        .c()
        .gateway
        .receive(f.message("approval-ask", "创建文件"))
        .await
        .unwrap();
    let card = f.card().await;
    let code = &card.request_id.simple().to_string()[..8];
    f.c()
        .gateway
        .receive(f.message("approval-detail", &format!("/详情 {code}")))
        .await
        .unwrap();
    f.c().gateway.dispatch().await.unwrap();
    assert!(last_notice(&f).contains("PRIVATE_TOOL_CONTENT"));
    assert!(!f.workspaces[0].path().join("agent.txt").exists());
    assert_eq!(f.script.requests.lock().len(), 1);
    let mut wrong = f.message("wrong-approve", &format!("/允许 {code}"));
    wrong.route.identity.external_user_id = "another-member".into();
    f.c().gateway.receive(wrong).await.unwrap();
    assert!(!f.workspaces[0].path().join("agent.txt").exists());
    let message = f.message("text-allow", &format!("/允许一次 {code}"));
    f.c().gateway.receive(message.clone()).await.unwrap();
    f.c().gateway.receive(message).await.unwrap();
    f.settled(receipt.session_id.unwrap(), receipt.turn_id.unwrap())
        .await;
    assert_eq!(
        std::fs::read_to_string(f.workspaces[0].path().join("agent.txt")).unwrap(),
        "PRIVATE_TOOL_CONTENT"
    );
    f.c()
        .gateway
        .receive(f.message("already-allowed", &format!("/允许 {code}")))
        .await
        .unwrap();
    f.c().gateway.dispatch().await.unwrap();
    assert!(last_notice(&f).contains("过期"));
    assert_eq!(f.script.requests.lock().len(), 2);
    f.finish().await;
}

#[tokio::test]
async fn qq_personal_settings_share_portal_data_and_stale_clear_cannot_erase_updates() {
    let f = Fixture::new(Vec::new()).await;
    f.pair().await;
    let settings = qq_menu(&f, "personal-menu", "/设置").await;
    assert_eq!(settings.buttons.len(), 4);
    f.c()
        .gateway
        .receive(f.message("remember", "/记住 生产环境先只读查看"))
        .await
        .unwrap();
    f.c()
        .gateway
        .receive(f.message("preferences", "/偏好 设置 用中文，先给结论"))
        .await
        .unwrap();
    let saved = f
        .c()
        .journal
        .user_data(f.c().browser.principal().id)
        .await
        .unwrap();
    assert_eq!(saved.memory, "生产环境先只读查看");
    assert_eq!(saved.instructions, "用中文，先给结论");
    let prompt = qq_menu(&f, "clear-confirm", "/记忆 清空").await;
    let old = button(&prompt, "确认清空");
    let mut updated = saved;
    updated.memory.push_str("\n网页的新记录");
    f.c()
        .journal
        .save_user_data(f.c().browser.principal().id, updated, 8192)
        .await
        .unwrap();
    f.c()
        .gateway
        .receive(f.message("stale-clear", &format!("/卡片 {old}")))
        .await
        .unwrap();
    let updated = f
        .c()
        .journal
        .user_data(f.c().browser.principal().id)
        .await
        .unwrap();
    assert!(updated.memory.contains("网页的新记录"));
    let memory = qq_menu(&f, "memory-view", "/记忆").await;
    assert!(memory.text.contains("网页的新记录"));
    let prompt = qq_menu(&f, "fresh-clear", "/记忆 清空").await;
    f.c()
        .gateway
        .receive(f.message(
            "confirmed-clear",
            &format!("/卡片 {}", button(&prompt, "确认清空")),
        ))
        .await
        .unwrap();
    let updated = f
        .c()
        .journal
        .user_data(f.c().browser.principal().id)
        .await
        .unwrap();
    assert!(updated.memory.is_empty());
    assert_eq!(updated.instructions, "用中文，先给结论");
    let usage = qq_menu(&f, "usage-view", "/用量").await;
    assert!(usage.text.contains("模型请求：0"));
    assert!(f.script.requests.lock().is_empty());
    f.finish().await;
}

#[tokio::test]
async fn qq_switches_personal_model_without_accepting_or_displaying_credentials() {
    let f = Fixture::new(Vec::new()).await;
    f.pair().await;
    let models = peri_cloud::models::AccountModels::new(
        f.c().journal.clone(),
        vec![peri_cloud::models::ModelProvider {
            id: "fixture".into(),
            name: "Fixture".into(),
            api_base: "http://127.0.0.1/v1".parse().unwrap(),
        }],
        "service-model".into(),
        4096,
    )
    .unwrap();
    f.connector.set_account_models(models.clone());
    models
        .save(
            f.c().browser.principal().id,
            peri_cloud::models::ModelUpdate {
                provider: "fixture".into(),
                model: "first-model".into(),
                api_key: Some("PRIVATE_FIXTURE_API_KEY".into()),
            },
        )
        .await
        .unwrap();
    f.c()
        .gateway
        .receive(f.message("model-change", "/模型 second-model"))
        .await
        .unwrap();
    let menu = qq_menu(&f, "model-view", "/模型").await;
    assert!(menu.text.contains("second-model"));
    assert!(!menu.text.contains("PRIVATE_FIXTURE_API_KEY"));
    assert_eq!(
        models
            .choice(f.c().browser.principal().id)
            .await
            .unwrap()
            .unwrap()
            .model,
        "second-model"
    );
    assert!(f.script.requests.lock().is_empty());
    f.finish().await;
}

#[tokio::test]
async fn qq_question_decline_is_explicit_and_does_not_send_an_empty_answer() {
    let f = Fixture::new(vec![question(true), answer("需要时再提供答案")]).await;
    f.pair().await;
    f.select("decline-select", 0).await;
    f.adapter.attempts.lock().clear();
    let receipt = f
        .c()
        .gateway
        .receive(f.message("decline-ask", "检查"))
        .await
        .unwrap();
    f.card().await;
    let menu = qq_menu(&f, "decline-reopen", "/问题").await;
    let done = qq_menu(
        &f,
        "decline-click",
        &format!("/卡片 {}", button(&menu, "暂不回答")),
    )
    .await;
    assert_eq!(done.title, "已暂不回答");
    f.settled(receipt.session_id.unwrap(), receipt.turn_id.unwrap())
        .await;
    assert!(f
        .c()
        .gateway
        .browser_interactions(&f.c().browser)
        .await
        .unwrap()
        .is_empty());
    assert!(f.script.requests.lock()[1]
        .to_string()
        .contains("用户拒绝回答"));
    f.finish().await;
}
