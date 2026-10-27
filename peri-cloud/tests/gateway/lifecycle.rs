use super::*;

#[tokio::test]
async fn gateway_pc_confirm_select_approve_once_real_write_and_only_ai_reply() {
    let f = Fixture::new(vec![main_tool_write(), answer("文件已写入。")]).await;
    f.pair().await;
    f.select("select-pc", 0).await;
    f.adapter.attempts.lock().clear();
    let message = f.message("chat-write", "在当前电脑创建文件");
    let receipt = f.c().gateway.receive(message.clone()).await.unwrap();
    assert_eq!(receipt.state, ReceiptState::Submitted);
    let card = f.card().await;
    assert_eq!(card.turn_id, receipt.turn_id.unwrap());
    assert_eq!(card.device_id, Some(f.devices[0].id));
    assert!(
        !f.workspaces[0].path().join("agent.txt").exists(),
        "批准前不得写入"
    );
    let mut wrong_route = f.route.clone();
    wrong_route.conversation_id = "another-chat".into();
    assert!(matches!(
        f.c()
            .gateway
            .respond(
                &wrong_route,
                card.request_id,
                InteractionAction::AllowOnce {}
            )
            .await,
        Err(GatewayError::StaleInteraction)
    ));
    f.c()
        .gateway
        .respond(&f.route, card.request_id, InteractionAction::AllowOnce {})
        .await
        .unwrap();
    assert!(
        matches!(
            f.c()
                .gateway
                .respond(&f.route, card.request_id, InteractionAction::AllowOnce {})
                .await,
            Err(GatewayError::StaleInteraction)
        ),
        "批准不能复用"
    );
    f.settled(card.session_id, card.turn_id).await;
    f.c().gateway.dispatch().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(f.workspaces[0].path().join("agent.txt")).unwrap(),
        "PRIVATE_TOOL_CONTENT"
    );
    assert!(
        !f.workspaces[1].path().join("agent.txt").exists(),
        "不能写到另一电脑"
    );
    let replies: Vec<_> = f
        .adapter
        .attempts
        .lock()
        .iter()
        .filter_map(|delivery| match &delivery.body {
            DeliveryBody::Reply { reply } => Some(reply.text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(replies, vec!["文件已写入。"], "普通聊天只包含 AI 文本");
    let duplicate = f.c().gateway.receive(message.clone()).await.unwrap();
    assert_eq!(duplicate.turn_id, receipt.turn_id);
    f.c().gateway.dispatch().await.unwrap();
    assert_eq!(
        f.script.requests.lock().len(),
        2,
        "平台重投不再调用模型或执行工具"
    );
    let mut conflicting = message;
    conflicting.text = "用同一平台消息ID换任务".into();
    assert!(matches!(
        f.c().gateway.receive(conflicting).await,
        Err(GatewayError::MessageConflict)
    ));
    let serialized = serde_json::to_value(f.adapter.attempts.lock().last().unwrap()).unwrap();
    assert!(serialized.get("internal_events").is_none());
    assert!(!serialized.to_string().contains("PRIVATE_REASONING"));
    f.finish().await;
}

#[tokio::test]
async fn gateway_busy_controls_and_cancel_reject_stale_approval() {
    let f = Fixture::new(vec![main_tool_write()]).await;
    f.pair().await;
    f.select("select-pc", 0).await;
    let receipt = f
        .c()
        .gateway
        .receive(f.message("chat-write", "创建文件"))
        .await
        .unwrap();
    let card = f.card().await;
    f.c()
        .gateway
        .receive(f.message("switch-busy", &format!("/连接 {}", f.devices[1].id)))
        .await
        .unwrap();
    f.c()
        .gateway
        .receive(f.message("mode-busy", "/权限 全部"))
        .await
        .unwrap();
    let before = f
        .c()
        .journal
        .session(f.c().browser.principal().id, card.session_id)
        .await
        .unwrap();
    assert_eq!(before.permission_mode, 0, "活动审批不改变权限快照");
    f.c()
        .gateway
        .receive(f.message("cancel", "/取消"))
        .await
        .unwrap();
    assert!(matches!(
        f.c()
            .gateway
            .respond(&f.route, card.request_id, InteractionAction::AllowOnce {})
            .await,
        Err(GatewayError::StaleInteraction)
    ));
    f.settled(card.session_id, receipt.turn_id.unwrap()).await;
    assert!(!f.workspaces[0].path().join("agent.txt").exists());
    assert!(!f.workspaces[1].path().join("agent.txt").exists());
    assert_eq!(f.script.requests.lock().len(), 1);
    f.finish().await;
}

#[tokio::test]
async fn gateway_device_revoke_invalidates_pending_approval() {
    let f = Fixture::new(vec![main_tool_write()]).await;
    f.pair().await;
    f.select("select-pc", 0).await;
    f.c()
        .gateway
        .receive(f.message("chat-write", "创建文件"))
        .await
        .unwrap();
    let card = f.card().await;
    f.c()
        .identity
        .revoke_device(&f.c().browser, card.device_id.unwrap())
        .await
        .unwrap();
    assert!(matches!(
        f.c()
            .gateway
            .respond(&f.route, card.request_id, InteractionAction::AllowOnce {})
            .await,
        Err(GatewayError::Identity(IdentityError::Forbidden))
    ));
    f.c()
        .runtime
        .cancel(f.c().browser.principal().id, card.session_id, card.turn_id)
        .await
        .unwrap();
    f.settled(card.session_id, card.turn_id).await;
    assert!(!f.workspaces[0].path().join("agent.txt").exists());
    f.finish().await;
}

#[tokio::test]
async fn gateway_cloud_restart_delivers_saved_ai_reply_and_target_switch_resets_policy() {
    let mut f = Fixture::new(vec![
        answer("第一台电脑完成。"),
        answer("继续使用第一台电脑。"),
        main_tool_write_for(1),
        answer("第二台电脑完成。"),
    ])
    .await;
    f.pair().await;
    f.select("select-first", 0).await;
    f.c()
        .gateway
        .receive(f.message("mode", "/权限 全部"))
        .await
        .unwrap();
    let receipt = f
        .c()
        .gateway
        .receive(f.message("first-chat", "回答当前任务"))
        .await
        .unwrap();
    let turn = receipt.turn_id.unwrap();
    let session = receipt.session_id.unwrap();
    f.settled(session, turn).await;
    f.adapter.attempts.lock().clear();
    f.restart().await;
    f.c().gateway.dispatch().await.unwrap();
    let replies: Vec<_> = f
        .adapter
        .attempts
        .lock()
        .iter()
        .filter_map(|delivery| match &delivery.body {
            DeliveryBody::Reply { reply } => Some(reply.text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        replies,
        vec!["第一台电脑完成。"],
        "新宿主投递已结算回复，不重新推理"
    );
    assert_eq!(f.script.requests.lock().len(), 1);
    let duplicate = f
        .c()
        .gateway
        .receive(f.message("first-chat", "回答当前任务"))
        .await
        .unwrap();
    assert_eq!(duplicate.turn_id, Some(turn));
    let resumed = f
        .c()
        .gateway
        .receive(f.message("resumed-chat", "继续上一个会话"))
        .await
        .unwrap();
    assert_eq!(resumed.session_id, Some(session), "重开宿主应重接原会话");
    f.settled(session, resumed.turn_id.unwrap()).await;
    assert_eq!(
        f.c()
            .journal
            .session(f.c().browser.principal().id, session)
            .await
            .unwrap()
            .permission_mode,
        4
    );
    let history = serde_json::to_string(&f.script.requests.lock()[1]).unwrap();
    assert!(
        history.contains("第一台电脑完成。"),
        "重开宿主应恢复 canonical 聊天历史"
    );
    f.select("select-second", 1).await;
    f.adapter.attempts.lock().clear();
    f.c()
        .gateway
        .receive(f.message("second-chat", "创建文件"))
        .await
        .unwrap();
    let card = f.card().await;
    assert_ne!(card.session_id, session);
    assert_eq!(card.device_id, Some(f.devices[1].id));
    assert_eq!(
        f.c()
            .journal
            .session(f.c().browser.principal().id, card.session_id)
            .await
            .unwrap()
            .permission_mode,
        0,
        "新电脑不继承全部允许"
    );
    f.c()
        .gateway
        .respond(&f.route, card.request_id, InteractionAction::AllowOnce {})
        .await
        .unwrap();
    f.settled(card.session_id, card.turn_id).await;
    assert!(!f.workspaces[0].path().join("agent.txt").exists());
    assert_eq!(
        std::fs::read_to_string(f.workspaces[1].path().join("agent.txt")).unwrap(),
        "PRIVATE_TOOL_CONTENT"
    );
    f.finish().await;
}

#[tokio::test]
async fn gateway_unknown_delivery_is_not_replayed_after_restart() {
    let mut f = Fixture::new(vec![]).await;
    f.adapter.fail_next.store(2, Ordering::SeqCst);
    f.c()
        .gateway
        .receive(f.message("help", "/帮助"))
        .await
        .unwrap();
    f.c().gateway.dispatch().await.unwrap();
    let first = f.adapter.attempts.lock()[0].clone();
    assert_eq!(first.message_sequence, 1);
    f.restart().await;
    f.c()
        .gateway
        .receive(f.message("help", "/帮助"))
        .await
        .unwrap();
    f.c().gateway.dispatch().await.unwrap();
    assert_eq!(
        f.adapter.attempts.lock().len(),
        1,
        "网络结果未知不能重复发送"
    );
    let encoded = serde_json::to_string(&first).unwrap();
    let decoded: Delivery = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded.delivery_id, first.delivery_id);
    assert_eq!(decoded.route, first.route);
    assert!(serde_json::from_value::<InboundMessage>(json!({"event_id":"missing-route"})).is_err());
    assert!(
        serde_json::from_value::<InteractionAction>(
            json!({"kind":"allow_once","allow_always":true})
        )
        .is_err(),
        "按钮协议不接受额外权限参数"
    );
    f.finish().await;
}

#[tokio::test]
async fn gateway_disconnected_dispatch_waiter_does_not_abort_owned_delivery() {
    let f = Fixture::new(vec![]).await;
    f.c()
        .gateway
        .receive(f.message("help", "/帮助"))
        .await
        .unwrap();
    f.adapter.block_next.store(true, Ordering::SeqCst);
    let gateway = f.c().gateway.clone();
    let waiter = tokio::spawn(async move { gateway.dispatch().await });
    tokio::time::timeout(Duration::from_secs(5), f.adapter.blocked.notified())
        .await
        .unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    assert!(
        !f.c().gateway.shutdown(Duration::ZERO).await.unwrap(),
        "发送仍在运行，不能假报关闭"
    );
    f.adapter.release.notify_one();
    assert!(f
        .c()
        .gateway
        .shutdown(Duration::from_secs(3))
        .await
        .unwrap());
    assert_eq!(f.adapter.attempts.lock().len(), 1);
    f.finish().await;
}

#[tokio::test]
async fn gateway_expired_card_and_revoked_channel_cannot_approve() {
    let f = Fixture::new(vec![main_tool_write()]).await;
    f.pair().await;
    f.select("select-pc", 0).await;
    f.c()
        .gateway
        .receive(f.message("chat-write", "创建文件"))
        .await
        .unwrap();
    let card = f.card().await;
    f.clock.0.fetch_add(301, Ordering::SeqCst);
    assert!(
        matches!(
            f.c()
                .gateway
                .respond(&f.route, card.request_id, InteractionAction::AllowOnce {})
                .await,
            Err(GatewayError::StaleInteraction)
        ),
        "有效期到达后拒绝授权"
    );
    f.c()
        .identity
        .revoke_channel(&f.c().browser, &f.route.identity)
        .await
        .unwrap();
    assert!(
        matches!(
            f.c()
                .gateway
                .receive(f.message("chat-write", "创建文件"))
                .await,
            Err(GatewayError::Identity(IdentityError::Unauthorized))
        ),
        "重投消息也不能绕过渠道撤销"
    );
    f.c()
        .gateway
        .receive(f.message("after-revoke", "/权限 全部"))
        .await
        .unwrap();
    assert_eq!(
        f.c()
            .journal
            .session(f.c().browser.principal().id, card.session_id)
            .await
            .unwrap()
            .permission_mode,
        0,
        "撤销渠道后不能修改权限"
    );
    f.c()
        .runtime
        .cancel(f.c().browser.principal().id, card.session_id, card.turn_id)
        .await
        .unwrap();
    f.settled(card.session_id, card.turn_id).await;
    assert!(!f.workspaces[0].path().join("agent.txt").exists());
    assert_eq!(f.script.requests.lock().len(), 1);
    f.finish().await;
}

#[tokio::test]
async fn gateway_second_router_cannot_recover_a_live_owner() {
    let f = Fixture::new(vec![]).await;
    let second = Gateway::new(
        f.c().runtime.clone(),
        f.c().identity.clone(),
        f.connector.clone(),
        vec![f.adapter.clone()],
        8,
    )
    .await;
    assert!(
        matches!(second, Err(GatewayError::Busy)),
        "第二个路由器不能恢复仍在运行的消息发送"
    );
    f.finish().await;
}

#[tokio::test]
async fn gateway_delivery_panic_keeps_health_failed_and_restart_preserves_unknown_send() {
    let mut f = Fixture::new(vec![]).await;
    f.c()
        .gateway
        .receive(f.message("help", "/帮助"))
        .await
        .unwrap();
    f.adapter.panic_next.store(true, Ordering::SeqCst);
    assert!(matches!(
        f.c().gateway.dispatch().await,
        Err(GatewayError::RecoveryRequired)
    ));
    assert!(
        matches!(
            f.c().gateway.shutdown(Duration::from_secs(3)).await,
            Err(GatewayError::RecoveryRequired)
        ),
        "panic 必须持续报告未确认状态"
    );
    assert!(
        matches!(
            f.c()
                .gateway
                .receive(f.message("blocked-after-panic", "/帮助"))
                .await,
            Err(GatewayError::RecoveryRequired)
        ),
        "下一次请求不能清除未确认状态"
    );
    let old = f.cloud.take().unwrap();
    assert!(
        old.runtime.shutdown(Duration::from_secs(5)).await.complete,
        "已完成的渠道句柄不阻止云运行退出"
    );
    drop(old);
    f.cloud = Some(
        CloudSide::start(
            f.root.path(),
            f.clock.clone(),
            f.connector.clone(),
            f.adapter.clone(),
            false,
            false,
        )
        .await,
    );
    f.c().gateway.dispatch().await.unwrap();
    assert_eq!(
        f.adapter.attempts.lock().len(),
        1,
        "sending 重开后保留结果未知，不重新发送"
    );
    f.c()
        .gateway
        .receive(f.message("new-help", "/帮助"))
        .await
        .unwrap();
    f.c().gateway.dispatch().await.unwrap();
    assert_eq!(f.adapter.attempts.lock().len(), 2, "恢复后允许新的独立消息");
    f.finish().await;
}

#[tokio::test]
async fn web_chat_persists_user_and_assistant_messages_and_advances_the_cursor() {
    let f = Fixture::new_for(vec![answer("WEB_ASSISTANT_REPLY")], true).await;
    let device = f.devices[0].id;
    let session = f
        .c()
        .gateway
        .web_select(&f.c().browser, Some(device), None)
        .await
        .unwrap();
    let sent = f
        .c()
        .gateway
        .web_send(&f.c().browser, "WEB_USER_MESSAGE")
        .await
        .unwrap();
    assert_eq!(sent.receipt.session_id, Some(session));
    f.settled(session, sent.receipt.turn_id.unwrap()).await;

    let mut projected = None;
    for _ in 0..200 {
        f.c().gateway.dispatch().await.unwrap();
        let current = f.c().gateway.web_chat(&f.c().browser, None).await.unwrap();
        if current.messages.iter().any(|message| {
            message.role == "assistant" && message.text.contains("WEB_ASSISTANT_REPLY")
        }) {
            projected = Some(current);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let chat = projected.expect("Web chat must project the assistant reply");
    assert!(chat.messages.iter().any(|message| message.role == "user"
        && message.text == "WEB_USER_MESSAGE"
        && message.state == "accepted"));
    assert_eq!(chat.session_id, Some(session));
    assert_eq!(chat.device_id, Some(device));

    let next = f
        .c()
        .gateway
        .web_chat(&f.c().browser, Some(chat.cursor))
        .await
        .unwrap();
    assert!(next.messages.is_empty(), "cursor must not replay history");
    assert_eq!(next.cursor, chat.cursor);
    f.finish().await;
}
