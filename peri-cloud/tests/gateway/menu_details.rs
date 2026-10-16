use super::*;

async fn command(f: &Fixture, event: &str, text: &str) -> Delivery {
    f.c().gateway.receive(f.message(event, text)).await.unwrap();
    f.c().gateway.dispatch().await.unwrap();
    f.adapter
        .attempts
        .lock()
        .iter()
        .rev()
        .find(|delivery| delivery.in_reply_to == event)
        .unwrap()
        .clone()
}

fn notice(delivery: Delivery) -> String {
    let DeliveryBody::Notice { text } = delivery.body else {
        panic!("expected notice")
    };
    text
}

#[tokio::test]
async fn full_question_menu_pages_pin_snapshot_enforce_route_and_do_not_consume_answers() {
    let label = format!("{}甲", "共同前缀完整选项".repeat(12));
    let description = format!("{}私有描述结尾", "每项说明必须完整保留。".repeat(240));
    let question = format!("{}完整题目结尾", "很长的题目背景。".repeat(80));
    let delta = json!({"choices":[{"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"question-menu","type":"function","function":{"name":"AskUserQuestion","arguments":json!({"questions":[{"question":question,"header":"环境","options":[{"label":label,"description":description},{"label":"另一个选项","description":"另一种选择"}],"multiSelect":false}]}).to_string()}}]},"finish_reason":"tool_calls"}]});
    let f = Fixture::new(vec![
        format!("data: {delta}\n\ndata: [DONE]\n\n"),
        answer("已收到完整选项"),
    ])
    .await;
    f.pair().await;
    f.select("select-long-menu", 0).await;
    let receipt = f
        .c()
        .gateway
        .receive(f.message("ask-long-menu", "选择环境"))
        .await
        .unwrap();
    let pending = f.card().await;
    f.c().gateway.dispatch().await.unwrap();
    let snapshot = f
        .adapter
        .attempts
        .lock()
        .iter()
        .rev()
        .find(|d| matches!(&d.body, DeliveryBody::Menu { card } if card.question_request.is_some()))
        .unwrap()
        .clone();
    let DeliveryBody::Menu { card } = &snapshot.body else {
        unreachable!()
    };
    assert!(card.text.contains(&question));
    assert!(card.text.contains(&description));
    assert_eq!(card.buttons[0].label, label);
    let first = notice(command(&f, "menu-full-first", "/菜单详情").await);
    assert!(first.contains(&format!("/菜单详情 {} 2", snapshot.delivery_id)));
    let pages = first
        .lines()
        .next()
        .unwrap()
        .split('/')
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse::<usize>()
        .unwrap();
    // A newer menu must not replace the original snapshot during pagination.
    command(&f, "newer-home-menu", "菜单").await;
    let mut all_pages = first;
    for page in 2..=pages {
        all_pages.push_str(&notice(
            command(
                &f,
                &format!("full-menu-{page}"),
                &format!("/菜单详情 {} {page}", snapshot.delivery_id),
            )
            .await,
        ));
    }
    assert!(all_pages.contains("私有描述结尾"));
    assert!(all_pages.contains("完整题目结尾"));
    assert!(notice(
        command(
            &f,
            "invalid-menu-page",
            &format!("/菜单详情 {} 0", snapshot.delivery_id)
        )
        .await
    )
    .contains("参数不正确"));
    let mut foreign = f.message(
        "foreign-group-menu",
        &format!("/菜单详情 {} 1", snapshot.delivery_id),
    );
    foreign.route.conversation_id = "group:another".into();
    f.c().gateway.receive(foreign).await.unwrap();
    f.c().gateway.dispatch().await.unwrap();
    let foreign = f.adapter.attempts.lock().last().unwrap().clone();
    assert!(notice(foreign).contains("没有可查看的菜单"));
    let mut foreign = f.message(
        "foreign-user-menu",
        &format!("/菜单详情 {} 1", snapshot.delivery_id),
    );
    foreign.route.identity.external_user_id = "unpaired-other-user".into();
    f.c().gateway.receive(foreign).await.unwrap();
    f.c().gateway.dispatch().await.unwrap();
    assert!(!notice(f.adapter.attempts.lock().last().unwrap().clone()).contains(&label));
    assert_eq!(
        f.script.requests.lock().len(),
        1,
        "reading menus never calls the model"
    );
    assert_eq!(
        f.c()
            .gateway
            .browser_interactions(&f.c().browser)
            .await
            .unwrap()[0]
            .request_id,
        pending.request_id
    );
    f.c()
        .gateway
        .receive(f.message(
            "choose-after-reading",
            &format!("/选 {} 1", snapshot.delivery_id),
        ))
        .await
        .unwrap();
    f.settled(receipt.session_id.unwrap(), receipt.turn_id.unwrap())
        .await;
    assert_eq!(f.script.requests.lock().len(), 2);
    f.finish().await;
}

#[tokio::test]
async fn history_menu_preserves_same_basename_paths_and_reaches_every_workspace() {
    let f = Fixture::new(Vec::new()).await;
    f.pair().await;
    f.select("select-history-menu", 0).await;
    let mut paths = Vec::new();
    for index in 0..10 {
        let path = f.workspaces[0]
            .path()
            .join(format!("server-{index}"))
            .join("same-project");
        std::fs::create_dir_all(&path).unwrap();
        let path = path.canonicalize().unwrap().to_str().unwrap().to_owned();
        command(&f, &format!("history-{index}"), &format!("/工作区 {path}")).await;
        paths.push(path);
    }
    let mut response = command(&f, "history-first-page", "/工作区").await;
    let mut seen = std::collections::HashSet::new();
    let mut page = 0;
    loop {
        page += 1;
        let DeliveryBody::Menu { card } = response.body else {
            panic!("expected menu")
        };
        for button in &card.buttons {
            if button.label.ends_with("same-project") {
                assert!(paths.contains(&button.label), "history must show full path");
                assert!(seen.insert(button.label.clone()));
            }
        }
        let Some(next) = card
            .buttons
            .iter()
            .find(|button| button.label == "下一页工作区")
        else {
            break;
        };
        response = command(
            &f,
            &format!("history-page-{page}"),
            &format!("/卡片 {}", next.action_id),
        )
        .await;
        assert!(page < 4);
    }
    assert_eq!(page, 2);
    assert_eq!(
        seen.len(),
        9,
        "all prior paths except current appear across pages"
    );
    assert!(f.script.requests.lock().is_empty());
    f.finish().await;
}
