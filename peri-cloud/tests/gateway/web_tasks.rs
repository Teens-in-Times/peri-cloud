use super::*;

#[tokio::test]
async fn browser_and_chat_share_task_admission_approval_results_and_idempotence() {
    let f = Fixture::new_for(
        vec![
            tool_write(),
            answer("写入成功。"),
            tool_write(),
            answer("已取消。"),
        ],
        true,
    )
    .await;
    // No QQ pairing or model call is needed to open a first-party workspace.
    let c = f.c();
    let request = Uuid::new_v4();
    let session = c
        .gateway
        .open_browser_workspace(&c.browser, request, f.devices[0].id, None)
        .await
        .unwrap();
    assert_eq!(
        c.gateway
            .open_browser_workspace(&c.browser, request, f.devices[0].id, None)
            .await
            .unwrap(),
        session
    );
    assert!(f.script.requests.lock().is_empty());
    assert_eq!(
        c.gateway
            .browser_workspaces(&c.browser)
            .await
            .unwrap()
            .len(),
        1
    );
    let task = Uuid::new_v4();
    let receipt = c
        .gateway
        .submit_browser_task(&c.browser, session, task, "写入验收文件".into())
        .await
        .unwrap();
    let duplicate = c
        .gateway
        .submit_browser_task(&c.browser, session, task, "写入验收文件".into())
        .await
        .unwrap();
    assert_eq!(receipt.turn_id, duplicate.turn_id);
    assert!(c
        .gateway
        .submit_browser_task(&c.browser, session, task, "不同任务".into())
        .await
        .is_err());
    let card = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(card) = c
                .gateway
                .browser_interactions(&c.browser)
                .await
                .unwrap()
                .into_iter()
                .next()
            {
                break card;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(card.session_id, session);
    assert!(!f.workspaces[0].path().join("agent.txt").exists());
    let invite = c.identity.create_invite(&c.browser).await.unwrap();
    c.identity
        .register_invited(&invite.code, "guest", PASSWORD.into(), "Guest")
        .await
        .unwrap();
    let login = c.identity.login("guest", PASSWORD.into()).await.unwrap();
    let guest = c
        .identity
        .authenticate_browser(&login.session_token)
        .await
        .unwrap();
    assert!(c
        .gateway
        .submit_browser_task(&guest, session, Uuid::new_v4(), "intrusion".into())
        .await
        .is_err());
    assert!(c
        .gateway
        .respond_browser(&guest, card.request_id, InteractionAction::AllowOnce {})
        .await
        .is_err());
    c.gateway
        .respond_browser(&c.browser, card.request_id, InteractionAction::AllowOnce {})
        .await
        .unwrap();
    f.settled(session, receipt.turn_id).await;
    assert_eq!(
        std::fs::read_to_string(f.workspaces[0].path().join("agent.txt")).unwrap(),
        "PRIVATE_TOOL_CONTENT"
    );
    assert!(f.adapter.attempts.lock().is_empty());
    let result = c
        .gateway
        .browser_result(&c.browser, session, 1, None)
        .await
        .unwrap();
    assert_eq!(result.unwrap().text, "写入成功。");
    // The very same task API also operates an existing QQ selection.
    f.pair().await;
    f.select("chat-selection", 1).await;
    let selected = c
        .gateway
        .browser_workspaces(&c.browser)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.adapter == "fixture-qq")
        .unwrap();
    let receipt = c
        .gateway
        .submit_browser_task(
            &c.browser,
            selected.session_id,
            Uuid::new_v4(),
            "等待审批后取消".into(),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while c
            .gateway
            .browser_interactions(&c.browser)
            .await
            .unwrap()
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    c.gateway
        .receive(f.message("chat-cancel", "/取消"))
        .await
        .unwrap();
    f.settled(selected.session_id, receipt.turn_id).await;
    assert!(!f.workspaces[1].path().join("agent.txt").exists());
    f.finish().await;
}
