use super::*;
use peri_cloud::scheduler::{NewSchedule, Notify, Schedule, ScheduleMode, Timing};

async fn selected(f: &Fixture) -> Uuid {
    f.pair().await;
    f.select("schedule-select", 0).await;
    f.c()
        .gateway
        .browser_workspaces(&f.c().browser)
        .await
        .unwrap()[0]
        .session_id
}
fn once(f: &Fixture, session: Uuid, mode: ScheduleMode) -> NewSchedule {
    NewSchedule {
        request_id: Uuid::new_v4(),
        session_id: session,
        title: "下午的问候".into(),
        prompt: "自然可爱地说下午好，问问我的心情。".into(),
        timing: Timing::Once {
            at: chrono::DateTime::from_timestamp(f.clock.unix_seconds() + 60, 0)
                .unwrap()
                .to_rfc3339(),
        },
        mode,
        notify: Notify::Always,
    }
}
async fn fire(f: &Fixture, task: &Schedule) -> peri_cloud::scheduler::ScheduleRun {
    f.clock.0.store(task.next_fire.unwrap(), Ordering::SeqCst);
    f.c().gateway.schedule_tick().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            f.c().gateway.dispatch().await.unwrap();
            let runs = f
                .c()
                .gateway
                .browser_schedule_runs(&f.c().browser, task.id)
                .await
                .unwrap();
            if let Some(run) = runs
                .into_iter()
                .find(|r| !matches!(r.state.as_str(), "claimed" | "submitted" | "deferred"))
            {
                return run;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .expect("scheduled turn must settle")
}

#[tokio::test]
async fn schedule_greeting_is_idempotent_proactive_and_conversation_continues() {
    let f = Fixture::new(vec![
        answer("下午好呀 ☀️ 今天心情怎么样？"),
        answer("听到你今天不错，我也很开心。"),
    ])
    .await;
    let session = selected(&f).await;
    let input = once(&f, session, ScheduleMode::Conversation);
    let task = f
        .c()
        .gateway
        .create_browser_schedule(&f.c().browser, input.clone())
        .await
        .unwrap();
    assert_eq!(
        f.c()
            .gateway
            .create_browser_schedule(&f.c().browser, input.clone())
            .await
            .unwrap()
            .id,
        task.id
    );
    let mut conflict = input;
    conflict.prompt = "different".into();
    assert!(f
        .c()
        .gateway
        .create_browser_schedule(&f.c().browser, conflict)
        .await
        .is_err());
    f.adapter.attempts.lock().clear();
    let run = fire(&f, &task).await;
    assert_eq!(run.state, "completed");
    assert_eq!(run.delivery_state, "delivered");
    f.c().gateway.schedule_tick().await.unwrap();
    f.c().gateway.dispatch().await.unwrap();
    let sends = f.adapter.attempts.lock().clone();
    assert_eq!(sends.len(), 1);
    assert_eq!(
        sends[0].reply_kind,
        peri_cloud::gateway::ReplyKind::Proactive
    );
    assert_eq!(sends[0].route, f.route);
    assert!(matches!(&sends[0].body,DeliveryBody::Reply {reply} if reply.text.contains("心情")));
    let requests = f.script.requests.lock().clone();
    assert_eq!(requests.len(), 1);
    let names: Vec<_> = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(
        !names.contains(&"Bash")
            && !names.contains(&"Write")
            && !names.contains(&"schedule_create")
            && !names.contains(&"schedule_update")
    );
    assert!(requests[0].to_string().contains("not a new user message"));
    let next = f
        .c()
        .gateway
        .receive(f.message("greeting-reply", "今天还不错！"))
        .await
        .unwrap();
    f.settled(session, next.turn_id.unwrap()).await;
    let last = f.script.requests.lock().last().unwrap().to_string();
    assert!(
        last.contains("今天心情怎么样") && last.contains("今天还不错"),
        "proactive message must be part of the real conversation history"
    );
    f.finish().await;
}

#[tokio::test]
async fn schedule_survives_restart_and_conversation_needs_no_live_executor() {
    let mut f = Fixture::new(vec![
        answer("下午好，来陪你聊一会儿。"),
        answer("我在呢，继续聊吧。"),
    ])
    .await;
    let session = selected(&f).await;
    let task = f
        .c()
        .gateway
        .create_browser_schedule(
            &f.c().browser,
            once(&f, session, ScheduleMode::Conversation),
        )
        .await
        .unwrap();
    let server = f.servers.remove(1);
    server.abort();
    assert!(server.await.is_err());
    f.restart().await;
    assert_eq!(
        f.c()
            .gateway
            .browser_schedules(&f.c().browser)
            .await
            .unwrap()
            .len(),
        1
    );
    let run = fire(&f, &task).await;
    assert_eq!(run.state, "completed");
    let state = f
        .c()
        .journal
        .session(f.c().browser.principal().id, session)
        .await
        .unwrap();
    assert!(
        state.execution_settled(),
        "a conversation with no executor tools settles without claiming an online device"
    );
    let reply = f
        .c()
        .gateway
        .receive(f.message("offline-reply", "今天不错"))
        .await
        .unwrap();
    f.settled(session, reply.turn_id.unwrap()).await;
    assert_eq!(
        f.script.requests.lock().len(),
        2,
        "reply continues with no live executor"
    );
    f.finish().await;
}

#[tokio::test]
async fn unknown_schedule_delivery_is_visible_and_never_retried_even_after_restart() {
    let mut f = Fixture::new(vec![answer("下午好。")]).await;
    let session = selected(&f).await;
    let task = f
        .c()
        .gateway
        .create_browser_schedule(
            &f.c().browser,
            once(&f, session, ScheduleMode::Conversation),
        )
        .await
        .unwrap();
    f.adapter.attempts.lock().clear();
    f.adapter.fail_next.store(2, Ordering::SeqCst);
    let run = fire(&f, &task).await;
    assert_eq!(run.state, "completed");
    assert_eq!(run.delivery_state, "unknown");
    f.restart().await;
    f.c().gateway.schedule_tick().await.unwrap();
    f.c().gateway.dispatch().await.unwrap();
    assert_eq!(f.adapter.attempts.lock().len(), 1);
    assert_eq!(f.script.requests.lock().len(), 1);
    f.finish().await;
}

#[tokio::test]
async fn schedule_task_preserves_exact_approval_cancel_and_device_identity() {
    let f = Fixture::new(vec![
        main_tool_write(),
        answer("已经写入。"),
        main_tool_write(),
        answer("停止处理。"),
    ])
    .await;
    let session = selected(&f).await;
    let mut input = once(&f, session, ScheduleMode::Task);
    input.prompt = "写入测试文件".into();
    let task = f
        .c()
        .gateway
        .create_browser_schedule(&f.c().browser, input)
        .await
        .unwrap();
    f.clock.0.store(task.next_fire.unwrap(), Ordering::SeqCst);
    f.c().gateway.schedule_tick().await.unwrap();
    let card = f.card().await;
    assert_eq!(card.session_id, session);
    assert_eq!(card.device_id, Some(f.devices[0].id));
    assert!(!f.workspaces[0].path().join("agent.txt").exists());
    f.c()
        .gateway
        .respond(&f.route, card.request_id, InteractionAction::AllowOnce {})
        .await
        .unwrap();
    f.settled(session, card.turn_id).await;
    f.c().gateway.dispatch().await.unwrap();
    assert!(f.workspaces[0].path().join("agent.txt").exists());
    let second = f
        .c()
        .gateway
        .create_browser_schedule(&f.c().browser, once(&f, session, ScheduleMode::Task))
        .await
        .unwrap();
    f.clock.0.store(second.next_fire.unwrap(), Ordering::SeqCst);
    f.c().gateway.schedule_tick().await.unwrap();
    let card2 = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(card) = f
                .c()
                .gateway
                .browser_interactions(&f.c().browser)
                .await
                .unwrap()
                .into_iter()
                .find(|c| c.turn_id != card.turn_id)
            {
                break card;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    f.c()
        .gateway
        .cancel_browser_task(&f.c().browser, session, card2.turn_id)
        .await
        .unwrap();
    f.settled(session, card2.turn_id).await;
    assert!(f
        .c()
        .gateway
        .respond(&f.route, card2.request_id, InteractionAction::AllowOnce {})
        .await
        .is_err());
    f.finish().await;
}

#[tokio::test]
async fn schedule_selection_revocation_misfire_and_silence_are_explicit() {
    let f = Fixture::new(vec![
        answer("换了电脑也会来问候你。"),
        answer("[SILENT]"),
        answer("设备撤销也不影响纯聊天。"),
    ])
    .await;
    let session = selected(&f).await;
    let task = f
        .c()
        .gateway
        .create_browser_schedule(
            &f.c().browser,
            once(&f, session, ScheduleMode::Conversation),
        )
        .await
        .unwrap();
    f.select("switch-schedule-target", 1).await;
    let run = fire(&f, &task).await;
    assert_eq!(run.state, "completed");
    assert_ne!(run.session_id, Some(session));
    assert_eq!(f.script.requests.lock().len(), 1);
    let current = f
        .c()
        .gateway
        .browser_workspaces(&f.c().browser)
        .await
        .unwrap()[0]
        .session_id;
    let missed = f
        .c()
        .gateway
        .create_browser_schedule(
            &f.c().browser,
            once(&f, current, ScheduleMode::Conversation),
        )
        .await
        .unwrap();
    f.clock
        .0
        .store(missed.next_fire.unwrap() + 301, Ordering::SeqCst);
    f.c().gateway.schedule_tick().await.unwrap();
    assert_eq!(
        f.c()
            .gateway
            .browser_schedule_runs(&f.c().browser, missed.id)
            .await
            .unwrap()[0]
            .reason
            .as_deref(),
        Some("misfire_expired")
    );
    let mut policy = f
        .c()
        .gateway
        .browser_contact_policy(&f.c().browser)
        .await
        .unwrap();
    policy.start_hour = 0;
    policy.end_hour = 24;
    f.c()
        .gateway
        .update_browser_contact_policy(&f.c().browser, policy)
        .await
        .unwrap();
    let mut input = once(&f, current, ScheduleMode::Conversation);
    input.notify = Notify::WhenUseful;
    let quiet = f
        .c()
        .gateway
        .create_browser_schedule(&f.c().browser, input)
        .await
        .unwrap();
    f.adapter.attempts.lock().clear();
    let run = fire(&f, &quiet).await;
    assert_eq!(run.delivery_state, "suppressed");
    assert!(f.adapter.attempts.lock().is_empty());
    let revoked = f
        .c()
        .gateway
        .create_browser_schedule(
            &f.c().browser,
            once(&f, current, ScheduleMode::Conversation),
        )
        .await
        .unwrap();
    f.c()
        .identity
        .revoke_device(&f.c().browser, f.devices[1].id)
        .await
        .unwrap();
    let run = fire(&f, &revoked).await;
    assert_eq!(
        run.state, "completed",
        "chat schedules do not require a still-enrolled device"
    );
    f.finish().await;
}

#[tokio::test]
async fn schedule_permissions_pause_cas_quota_and_owner_isolation() {
    let f = Fixture::new(vec![]).await;
    let session = selected(&f).await;
    let input = once(&f, session, ScheduleMode::Conversation);
    let task = f
        .c()
        .gateway
        .create_browser_schedule(&f.c().browser, input)
        .await
        .unwrap();
    let paused = f
        .c()
        .gateway
        .change_browser_schedule(&f.c().browser, task.id, task.revision, "pause")
        .await
        .unwrap();
    assert!(!paused.enabled);
    assert!(f
        .c()
        .gateway
        .change_browser_schedule(&f.c().browser, task.id, task.revision, "resume")
        .await
        .is_err());
    f.clock.0.store(task.next_fire.unwrap(), Ordering::SeqCst);
    f.c().gateway.schedule_tick().await.unwrap();
    assert!(f
        .c()
        .gateway
        .browser_schedule_runs(&f.c().browser, task.id)
        .await
        .unwrap()
        .is_empty());
    let invite = f.c().identity.create_invite(&f.c().browser).await.unwrap();
    f.c()
        .identity
        .register_invited(&invite.code, "other", PASSWORD.into(), "Other")
        .await
        .unwrap();
    let login = f
        .c()
        .identity
        .login("other", PASSWORD.into())
        .await
        .unwrap();
    let guest = f
        .c()
        .identity
        .authenticate_browser(&login.session_token)
        .await
        .unwrap();
    assert!(f
        .c()
        .gateway
        .browser_schedules(&guest)
        .await
        .unwrap()
        .is_empty());
    assert!(f
        .c()
        .gateway
        .change_browser_schedule(&guest, task.id, paused.revision, "delete")
        .await
        .is_err());
    assert!(f
        .c()
        .gateway
        .browser_schedule_runs(&guest, task.id)
        .await
        .is_err());
    assert!(f
        .c()
        .gateway
        .create_browser_schedule(&guest, once(&f, session, ScheduleMode::Conversation))
        .await
        .is_err());
    for _ in 1..20 {
        f.c()
            .gateway
            .create_browser_schedule(
                &f.c().browser,
                once(&f, session, ScheduleMode::Conversation),
            )
            .await
            .unwrap();
    }
    assert!(f
        .c()
        .gateway
        .create_browser_schedule(
            &f.c().browser,
            once(&f, session, ScheduleMode::Conversation)
        )
        .await
        .is_err());
    let deleted = f
        .c()
        .gateway
        .change_browser_schedule(&f.c().browser, task.id, paused.revision, "delete")
        .await
        .unwrap();
    assert!(deleted.deleted);
    assert!(f
        .c()
        .gateway
        .change_browser_schedule(&f.c().browser, task.id, deleted.revision, "resume")
        .await
        .is_err());
    f.finish().await;
}

#[tokio::test]
async fn schedule_pure_cloud_chat_without_any_registered_device_keeps_tools_scoped() {
    let f = Fixture::new(vec![answer("下午好呀。"), answer("今天想聊点什么？")]).await;
    f.pair().await;
    for device in &f.devices {
        f.c()
            .identity
            .revoke_device(&f.c().browser, device.id)
            .await
            .unwrap();
    }
    let at = chrono::DateTime::from_timestamp(f.clock.unix_seconds() + 60, 0)
        .unwrap()
        .to_rfc3339();
    f.c()
        .gateway
        .receive(f.message(
            "cloud-schedule-create",
            &format!("/定时 一次 {at} 和我说下午好"),
        ))
        .await
        .unwrap();
    let task = f
        .c()
        .gateway
        .browser_schedules(&f.c().browser)
        .await
        .unwrap()
        .remove(0);
    assert!(task.device_id.is_none());
    let session = task.session_id;
    let state = f
        .c()
        .journal
        .session(f.c().browser.principal().id, session)
        .await
        .unwrap();
    assert!(state.frozen.device_id.is_none() && state.frozen.binding.is_none());
    f.adapter.attempts.lock().clear();
    let run = fire(&f, &task).await;
    assert_eq!(run.delivery_state, "delivered");
    let reply = f
        .c()
        .gateway
        .receive(f.message("pure-cloud-reply", "心情不错"))
        .await
        .unwrap();
    f.settled(session, reply.turn_id.unwrap()).await;
    {
        let requests = f.script.requests.lock();
        let last = requests.last().unwrap();
        let names: Vec<_> = last["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
            .collect();
        assert!(names.contains(&"schedule_create") && names.contains(&"schedule_remove"));
        assert!(!names.contains(&"Bash"));
        assert!(last.to_string().contains("下午好呀"));
    }
    f.finish().await;
}
#[tokio::test]
async fn schedule_contact_optout_and_clear_suppress_generated_unsent_message() {
    for cleared in [false, true] {
        let f = Fixture::new(vec![answer("这条旧消息不应发送。")]).await;
        let session = selected(&f).await;
        let task = f
            .c()
            .gateway
            .create_browser_schedule(
                &f.c().browser,
                once(&f, session, ScheduleMode::Conversation),
            )
            .await
            .unwrap();
        f.adapter.attempts.lock().clear();
        f.clock.0.store(task.next_fire.unwrap(), Ordering::SeqCst);
        f.c().gateway.schedule_tick().await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let run = f
                    .c()
                    .gateway
                    .browser_schedule_runs(&f.c().browser, task.id)
                    .await
                    .unwrap()
                    .remove(0);
                if let Some(turn) = run.turn_id {
                    if f.c()
                        .journal
                        .turn(f.c().browser.principal().id, run.session_id.unwrap(), turn)
                        .await
                        .unwrap()
                        .state
                        == TurnState::Completed
                    {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        if cleared {
            let receipt = f
                .c()
                .gateway
                .receive(f.message("clear-before-send", "/clear"))
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if f.c()
                        .journal
                        .turn(
                            f.c().browser.principal().id,
                            session,
                            receipt.turn_id.unwrap(),
                        )
                        .await
                        .unwrap()
                        .state
                        == TurnState::Completed
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        } else {
            let mut policy = f
                .c()
                .gateway
                .browser_contact_policy(&f.c().browser)
                .await
                .unwrap();
            policy.enabled = false;
            f.c()
                .gateway
                .update_browser_contact_policy(&f.c().browser, policy)
                .await
                .unwrap();
        }
        f.c().gateway.dispatch().await.unwrap();
        assert!(!f
            .adapter
            .attempts
            .lock()
            .iter()
            .any(|d| d.reply_kind == peri_cloud::gateway::ReplyKind::Proactive));
        let run = f
            .c()
            .gateway
            .browser_schedule_runs(&f.c().browser, task.id)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(run.delivery_state, "suppressed");
        assert_eq!(f.script.requests.lock().len(), 1);
        f.finish().await;
    }
}

#[tokio::test]
async fn schedule_cloud_conversation_retains_question_answers_and_history_controls() {
    let f = Fixture::new(vec![
        super::experience_tests::question(false),
        answer("收到，检查 A。"),
    ])
    .await;
    f.pair().await;
    for device in &f.devices {
        f.c()
            .identity
            .revoke_device(&f.c().browser, device.id)
            .await
            .unwrap();
    }
    let receipt = f
        .c()
        .gateway
        .receive(f.message("cloud-question", "请帮我选一下"))
        .await
        .unwrap();
    let card = f.card().await;
    assert!(card.device_id.is_none());
    f.c()
        .gateway
        .receive(f.message("cloud-question-answer", "/回答 1"))
        .await
        .unwrap();
    f.settled(card.session_id, receipt.turn_id.unwrap()).await;
    assert_eq!(f.script.requests.lock().len(), 2);
    f.c()
        .gateway
        .receive(f.message("cloud-history", "/history"))
        .await
        .unwrap();
    f.c().gateway.dispatch().await.unwrap();
    assert!(f
        .adapter
        .attempts
        .lock()
        .iter()
        .any(|d| matches!(&d.body,DeliveryBody::Notice {text} if text.contains("历史会话"))));
    assert_eq!(
        f.script.requests.lock().len(),
        2,
        "history must not call the model"
    );
    f.finish().await;
}
