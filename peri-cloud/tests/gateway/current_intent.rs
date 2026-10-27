use super::execution_session_tests::{call, pump, tool_value};
use super::*;
use peri_cloud::memory::{MemoryContent, MemoryKind, MemorySource, MemoryStore};

async fn pool(f: &Fixture) -> sqlx::SqlitePool {
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(f.root.path().join("cloud.sqlite"))
                .foreign_keys(true),
        )
        .await
        .unwrap()
}
async fn chat(f: &Fixture, session: Uuid, text: &str) {
    let receipt = f
        .c()
        .gateway
        .submit_browser_task(&f.c().browser, session, Uuid::new_v4(), text.into())
        .await
        .unwrap_or_else(|error| panic!("chat {text:?} failed: {error}"));
    f.settled(session, receipt.turn_id).await;
}
async fn frame(f: &Fixture, session: Uuid) -> Value {
    let state = f
        .c()
        .journal
        .session(f.c().browser.principal().id, session)
        .await
        .unwrap();
    state
        .history
        .iter()
        .rev()
        .find_map(|p| {
            if state
                .history_flags
                .get(&p.id())
                .is_some_and(|flags| flags.excluded || flags.truncated)
            {
                return None;
            }
            let peri_acp_types::store::PersistedPayload::SystemReminder { reminder, .. } = p else {
                return None;
            };
            if reminder.as_reminder().source.0 != "cloud_tasks" {
                return None;
            }
            let (_, body) = reminder.as_reminder().body.split_once('\n')?;
            let body: Value = serde_json::from_str(body).ok()?;
            body.get("current_intent").cloned()
        })
        .unwrap()
}
async fn goal(db: &sqlx::SqlitePool, owner: Uuid, session: Uuid, objective: &str) {
    let mut goal = peri_acp_types::goal::ThreadGoal::new_at(
        objective.into(),
        None,
        std::time::SystemTime::now(),
    );
    goal.status = peri_acp_types::goal::GoalStatus::Complete;
    sqlx::query("INSERT INTO cloud_goals VALUES(?,?,?,?)")
        .bind(session.to_string())
        .bind(owner.to_string())
        .bind(serde_json::to_string(&goal).unwrap())
        .bind(1i64)
        .execute(db)
        .await
        .unwrap();
}

#[tokio::test]
async fn current_intent_sources_expiry_forget_and_restart_stay_conversation_scoped() {
    let mut f = Fixture::new_for(
        vec![
            call("inspect_intent", json!({}), "intent-first"),
            answer("current sources observed"),
        ],
        true,
    )
    .await;
    let owner = f.c().browser.principal().id;
    let session = f
        .c()
        .gateway
        .open_browser_workspace(&f.c().browser, Uuid::new_v4(), f.devices[0].id, None)
        .await
        .unwrap();
    let other = f
        .c()
        .gateway
        .open_browser_workspace(&f.c().browser, Uuid::new_v4(), f.devices[1].id, None)
        .await
        .unwrap();
    let db = pool(&f).await;
    goal(&db, owner, session, "COMPLETED_CURRENT_OBJECTIVE").await;
    goal(&db, owner, other, "OTHER_CONVERSATION_OBJECTIVE").await;
    f.c()
        .journal
        .save_preferences(
            owner,
            peri_cloud::state::UserPreferences {
                revision: 0,
                instructions: "EXPLICIT_USER_PREFERENCE".into(),
            },
        )
        .await
        .unwrap();
    let memory = MemoryStore::new(&f.c().journal, owner, 65536);
    let item = memory
        .add(
            Uuid::new_v4(),
            0,
            MemoryContent {
                content: "RECORDED_FACT".into(),
                kind: MemoryKind::Decision,
                pinned: true,
                valid_until: None,
            },
            MemorySource::web(),
        )
        .await
        .unwrap();
    let expired = memory
        .add(
            Uuid::new_v4(),
            1,
            MemoryContent {
                content: "EXPIRED_FACT".into(),
                kind: MemoryKind::Fact,
                pinned: true,
                valid_until: None,
            },
            MemorySource::web(),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE memory_items SET valid_until=0 WHERE id=?")
        .bind(expired.id.to_string())
        .execute(&db)
        .await
        .unwrap();
    for (id, title) in [(session, "CURRENT_COMMITMENT"), (other, "OTHER_COMMITMENT")] {
        f.c()
            .gateway
            .create_browser_schedule(
                &f.c().browser,
                peri_cloud::scheduler::NewSchedule {
                    request_id: Uuid::new_v4(),
                    session_id: id,
                    title: title.into(),
                    prompt: "Explicitly arranged future work".into(),
                    timing: peri_cloud::scheduler::Timing::Once {
                        at: chrono::DateTime::from_timestamp(f.clock.unix_seconds() + 3600, 0)
                            .unwrap()
                            .to_rfc3339(),
                    },
                    mode: peri_cloud::scheduler::ScheduleMode::Conversation,
                    notify: peri_cloud::scheduler::Notify::WhenUseful,
                },
            )
            .await
            .unwrap();
    }
    chat(&f, session, "what are you working on?").await;
    let view = tool_value(&f.script.requests.lock()[1], "schema");
    assert_eq!(view["session_id"], session.to_string());
    assert_eq!(view["goal"]["status"], "complete");
    assert_eq!(view["goal"]["completion_is_verified"], false);
    assert_eq!(view["active_tasks"]["items"], json!([]));
    assert_eq!(
        view["preferences"]["instructions"]["text"],
        "EXPLICIT_USER_PREFERENCE"
    );
    assert_eq!(
        view["memory_references"]["items"][0]["id"],
        item.id.to_string()
    );
    assert_eq!(
        view["memory_references"]["items"][0]["origin"]["kind"],
        "web"
    );
    assert_eq!(
        view["scheduled_commitments"]["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let text = view.to_string();
    for hidden in [
        "OTHER_CONVERSATION_OBJECTIVE",
        "OTHER_COMMITMENT",
        "EXPIRED_FACT",
    ] {
        assert!(!text.contains(hidden), "{hidden}");
    }
    assert!(text.contains("CURRENT_COMMITMENT"));
    assert!(serde_json::to_vec(&view).unwrap().len() < 13 * 1024);
    let host = frame(&f, session).await;
    assert_eq!(host["goal"], view["goal"]);
    memory
        .forget(item.id, item.revision, MemorySource::web())
        .await
        .unwrap();
    db.close().await;
    f.restart().await;
    f.script.replies.lock().extend([
        call("inspect_intent", json!({}), "intent-after-forget"),
        answer("forgotten source no longer active"),
    ]);
    chat(&f, session, "what still applies?").await;
    let current = frame(&f, session).await;
    assert_eq!(current["memory_references"]["items"], json!([]));
    let requests = f.script.requests.lock().clone();
    assert!(
        !requests[2]["messages"]
            .to_string()
            .contains("RECORDED_FACT"),
        "old intent and tool results must respect the forget barrier"
    );
    assert!(!requests[3]["messages"]
        .to_string()
        .contains("RECORDED_FACT"));
    // Continue ordinary conversation for several refresh intervals, without
    // creating work or recovering an invalidated source from older frames.
    for round in 0..12 {
        f.script
            .replies
            .lock()
            .push_back(answer("ordinary conversation continues"));
        chat(&f, session, &format!("casual conversation round {round}")).await;
        let current = frame(&f, session).await;
        assert_eq!(current["goal"]["id"], view["goal"]["id"]);
        assert_eq!(current["memory_references"]["items"], json!([]));
        assert_eq!(current["active_tasks"]["items"], json!([]));
    }
    assert_eq!(
        f.submissions[0].load(Ordering::SeqCst),
        0,
        "projection executes no device work"
    );
    f.finish().await;
}

#[tokio::test]
async fn current_intent_group_view_excludes_private_sources_and_other_accounts() {
    let f = Fixture::new(vec![
        call("inspect_intent", json!({}), "group-intent"),
        answer("group-only view"),
    ])
    .await;
    f.pair().await;
    let owner = f.c().browser.principal().id;
    f.c()
        .journal
        .save_preferences(
            owner,
            peri_cloud::state::UserPreferences {
                revision: 0,
                instructions: "PRIVATE_INTENT_PREFERENCE".into(),
            },
        )
        .await
        .unwrap();
    let invite = f.c().identity.create_invite(&f.c().browser).await.unwrap();
    let guest = f
        .c()
        .identity
        .register_invited(&invite.code, "intent-guest", PASSWORD.into(), "Guest")
        .await
        .unwrap();
    for (principal, marker) in [
        (owner, "PRIVATE_INTENT_FACT"),
        (guest.id, "OTHER_ACCOUNT_INTENT_FACT"),
    ] {
        MemoryStore::new(&f.c().journal, principal, 65536)
            .add(
                Uuid::new_v4(),
                0,
                MemoryContent {
                    content: marker.into(),
                    kind: MemoryKind::Fact,
                    pinned: true,
                    valid_until: None,
                },
                MemorySource::web(),
            )
            .await
            .unwrap();
    }
    let mut message = f.message("group-intent-device", &format!("/连接 {}", f.devices[0].id));
    message.route.conversation_id = "group:intent".into();
    f.c().gateway.receive(message.clone()).await.unwrap();
    f.c().gateway.dispatch().await.unwrap();
    message.event_id = "group-intent-message".into();
    message.text = "what are you doing in this conversation?".into();
    let receipt = f.c().gateway.receive(message).await.unwrap();
    let session = receipt.session_id.unwrap();
    f.settled(session, receipt.turn_id.unwrap()).await;
    let requests = f.script.requests.lock().clone();
    let view = tool_value(&requests[1], "schema");
    assert_eq!(view["private"], false);
    assert_eq!(view["preferences"], Value::Null);
    assert_eq!(view["contact_policy"], Value::Null);
    assert_eq!(view["memory_references"]["items"], json!([]));
    let serialized = serde_json::to_string(&requests).unwrap();
    for marker in [
        "PRIVATE_INTENT_PREFERENCE",
        "PRIVATE_INTENT_FACT",
        "OTHER_ACCOUNT_INTENT_FACT",
    ] {
        assert!(!serialized.contains(marker));
    }
    assert_eq!(frame(&f, session).await["private"], false);
    f.finish().await;
}

#[tokio::test]
async fn current_intent_keeps_chat_responsive_during_durable_question_and_separates_acceptance() {
    let mut f = Fixture::new_for(
        vec![
            call(
                "delegate_task",
                json!({"goal":"ask one question then finish"}),
                "intent-delegate",
            ),
            answer("delegated"),
            call(
                "message_parent",
                json!({"body":"which release should I target?","wait_for_reply":true}),
                "intent-question",
            ),
            answer("question received"),
        ],
        true,
    )
    .await;
    let parent = f
        .c()
        .gateway
        .open_browser_workspace(&f.c().browser, Uuid::new_v4(), f.devices[0].id, None)
        .await
        .unwrap();
    chat(&f, parent, "start the independent work").await;
    let receipt = super::execution_session_tests::delegate_receipt(&f.script.requests.lock()[1]);
    pump(&f, 4).await;
    assert!(
        !f.script.requests.lock()[2]["tools"]
            .to_string()
            .contains("inspect_intent"),
        "workers cannot read the main's intent view"
    );
    f.restart().await;
    f.c().gateway.dispatch().await.unwrap();
    assert_eq!(
        f.script.requests.lock().len(),
        4,
        "restart does not replay work or questions"
    );
    f.script.replies.lock().extend([
        call("inspect_intent", json!({}), "intent-waiting"),
        answer("we can keep chatting while that task waits"),
    ]);
    chat(&f, parent, "we can chat; what is waiting?").await;
    let waiting = tool_value(&f.script.requests.lock()[5], "schema");
    assert_eq!(
        waiting["active_tasks"]["items"][0]["state"],
        "waiting_parent"
    );
    assert_eq!(
        waiting["pending_replies"]["items"][0]["task_id"],
        receipt["task_id"]
    );
    assert!(waiting["pending_replies"]
        .to_string()
        .contains("which release"));
    let revision = waiting["active_tasks"]["items"][0]["revision"].clone();
    f.script.replies.lock().extend([
        call("message_task",json!({"task_id":receipt["task_id"],"expected_revision":revision,"body":"use the preview release"}),"intent-reply"),
        answer("reply recorded"),answer("worker finished"),answer("result is available for review"),
    ]);
    chat(&f, parent, "use the preview release").await;
    pump(&f, 10).await;
    f.script.replies.lock().extend([
        call("inspect_intent", json!({}), "intent-finished"),
        answer("execution ended, acceptance remains separate"),
    ]);
    chat(&f, parent, "where did the task end up?").await;
    let done = tool_value(&f.script.requests.lock()[11], "schema");
    assert_eq!(done["active_tasks"]["items"], json!([]));
    assert_eq!(done["pending_replies"]["items"], json!([]));
    assert_eq!(done["recent_outcomes"]["items"][0]["state"], "completed");
    assert_eq!(
        done["recent_outcomes"]["items"][0]["delivery"]["recorded_review"],
        Value::Null
    );
    assert!(done["recent_outcomes"]["items"][0]["latest_instruction"]
        .to_string()
        .contains("preview release"));
    assert_eq!(f.submissions[0].load(Ordering::SeqCst), 0);
    f.finish().await;
}

#[tokio::test]
async fn current_intent_rejects_model_supplied_scope_and_cancellation_is_not_active_work() {
    let f = Fixture::new_for(
        vec![
            call(
                "delegate_task",
                json!({"goal":"wait for external input"}),
                "intent-cancel-delegate",
            ),
            answer("delegated"),
            call(
                "wait_task",
                json!({"reason":"external condition"}),
                "intent-wait",
            ),
        ],
        true,
    )
    .await;
    let parent = f
        .c()
        .gateway
        .open_browser_workspace(&f.c().browser, Uuid::new_v4(), f.devices[0].id, None)
        .await
        .unwrap();
    chat(&f, parent, "delegate").await;
    pump(&f, 3).await;
    let db = pool(&f).await;
    let raw: String = sqlx::query_scalar("SELECT record_json FROM agent_tasks LIMIT 1")
        .fetch_one(&db)
        .await
        .unwrap();
    let record: Value = serde_json::from_str(&raw).unwrap();
    f.script.replies.lock().extend([
        call("control_task",json!({"task_id":record["id"],"expected_revision":record["revision"],"action":"cancel"}),"intent-cancel"),
        answer("cancel requested"),
        answer("cancellation result received"),
    ]);
    chat(&f, parent, "cancel that task").await;
    // Let the existing cancellation coordinator settle its own state.
    pump(&f, 6).await;
    f.script.replies.lock().extend([
        call("inspect_intent", json!({}), "intent-cancelled"),
        answer("cancelled work is historical"),
    ]);
    chat(&f, parent, "what is still active?").await;
    let view = frame(&f, parent).await;
    assert_eq!(view["active_tasks"]["items"], json!([]));
    assert_eq!(view["recent_outcomes"]["items"][0]["state"], "cancelled");
    f.script.replies.lock().extend([
        call(
            "inspect_intent",
            json!({"principal":Uuid::new_v4(),"session_id":Uuid::new_v4()}),
            "intent-forged-scope",
        ),
        answer("invalid scope rejected"),
    ]);
    chat(&f, parent, "invalid parameters fixture").await;
    {
        let requests = f.script.requests.lock();
        let last = requests.last().unwrap()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|message| message["role"] == "tool")
            .unwrap();
        assert!(last["content"].to_string().contains("invalid"));
    }
    db.close().await;
    f.finish().await;
}
