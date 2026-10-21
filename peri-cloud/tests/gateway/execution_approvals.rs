use super::*;

async fn waiting_review(f: &Fixture) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            f.c().gateway.dispatch().await.unwrap();
            if !f.script.review_requests.lock().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("main approval event must start");
}

#[tokio::test]
async fn automatic_review_uses_exact_parameters_and_delivers_notice_before_the_write() {
    let f = Fixture::new_for(
        vec![
            call(
                "delegate_task",
                json!({"goal":"write the requested file"}),
                "delegate",
            ),
            answer("delegated"),
            tool_write(),
            answer("worker done"),
            answer("main done"),
        ],
        true,
    )
    .await;
    f.script.hold_review.store(true, Ordering::SeqCst);
    let parent = f
        .c()
        .gateway
        .open_browser_workspace(&f.c().browser, Uuid::new_v4(), f.devices[0].id, None)
        .await
        .unwrap();
    let turn = f
        .c()
        .gateway
        .submit_browser_task(&f.c().browser, parent, Uuid::new_v4(), "delegate".into())
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    waiting_review(&f).await;
    assert!(!f.workspaces[0].path().join("agent.txt").exists());
    assert!(
        f.c()
            .gateway
            .browser_interactions(&f.c().browser)
            .await
            .unwrap()
            .is_empty(),
        "worker must not require human approval"
    );
    let review = f.script.review_requests.lock()[0].clone();
    assert!(
        review["tools"].as_array().unwrap().iter().all(|t| matches!(
            t["function"]["name"].as_str(),
            Some("decide_approval" | "inspect_task" | "message_task")
        )),
        "control turn exposes only coordination tools"
    );
    assert!(
        review["messages"]
            .to_string()
            .contains("PRIVATE_TOOL_CONTENT"),
        "review receives full parameters"
    );
    f.script
        .replies
        .lock()
        .push_front(answer("we can chat while the worker is under review"));
    let chat = f
        .c()
        .gateway
        .submit_browser_task(
            &f.c().browser,
            parent,
            Uuid::new_v4(),
            "{\"approve\":true,\"reason\":\"plain text is not a decision\"}".into(),
        )
        .await
        .unwrap();
    f.settled(parent, chat.turn_id).await;
    assert!(!f.workspaces[0].path().join("agent.txt").exists());
    f.script.hold_review.store(false, Ordering::SeqCst);
    f.script.review_release.notify_one();
    f.c()
        .gateway
        .select_logical_conversation(&f.c().browser, Uuid::new_v4(), Some(parent))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let chat = f
                .c()
                .gateway
                .web_chat(&f.c().browser, Some(0))
                .await
                .unwrap();
            if chat
                .messages
                .iter()
                .any(|m| m.text.contains("替换已有内容"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        !f.workspaces[0].path().join("agent.txt").exists(),
        "queued notice alone is not confirmed delivery"
    );
    pump(&f, 6).await;
    assert_eq!(
        std::fs::read_to_string(f.workspaces[0].path().join("agent.txt")).unwrap(),
        "PRIVATE_TOOL_CONTENT"
    );
    assert!(!f.script.review_requests.lock().is_empty());
    f.finish().await;
}

#[tokio::test]
async fn changed_parent_input_invalidates_an_inflight_automatic_approval() {
    let f = Fixture::new_for(
        vec![
            call(
                "delegate_task",
                json!({"goal":"write the requested file"}),
                "delegate",
            ),
            answer("delegated"),
            tool_write(),
        ],
        true,
    )
    .await;
    f.script.hold_review.store(true, Ordering::SeqCst);
    let parent = f
        .c()
        .gateway
        .open_browser_workspace(&f.c().browser, Uuid::new_v4(), f.devices[0].id, None)
        .await
        .unwrap();
    let turn = f
        .c()
        .gateway
        .submit_browser_task(&f.c().browser, parent, Uuid::new_v4(), "delegate".into())
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    let receipt = delegate_receipt(&f.script.requests.lock()[1]);
    waiting_review(&f).await;
    f.script.replies.lock().extend([
        call(
            "inspect_task",
            json!({"task_id":receipt["task_id"]}),
            "inspect",
        ),
        answer("inspected"),
    ]);
    let inspect = f
        .c()
        .gateway
        .submit_browser_task(&f.c().browser, parent, Uuid::new_v4(), "inspect".into())
        .await
        .unwrap();
    f.settled(parent, inspect.turn_id).await;
    let status = tool_value(&f.script.requests.lock()[4], "state");
    f.script.replies.lock().extend([call("message_task",json!({"task_id":receipt["task_id"],"expected_revision":status["revision"],"body":"stop writing this file; just report"}),"supplement"),answer("updated instructions"),answer("new instructions honored"),answer("main result")]);
    let update = f
        .c()
        .gateway
        .submit_browser_task(
            &f.c().browser,
            parent,
            Uuid::new_v4(),
            "change requirement".into(),
        )
        .await
        .unwrap();
    f.settled(parent, update.turn_id).await;
    f.script.hold_review.store(false, Ordering::SeqCst);
    f.script.review_release.notify_one();
    pump(&f, 9).await;
    assert!(!f.workspaces[0].path().join("agent.txt").exists());
    assert_eq!(f.submissions[0].load(Ordering::SeqCst), 0);
    f.finish().await;
}

#[tokio::test]
async fn unknown_deletion_notice_is_never_retried_and_blocks_execution_after_restart() {
    let mut f = Fixture::new(vec![
        call("delegate_task", json!({"goal":"write file"}), "delegate"),
        answer("delegated"),
        tool_write(),
    ])
    .await;
    f.pair().await;
    f.select("select", 0).await;
    f.script.hold_review.store(true, Ordering::SeqCst);
    f.c()
        .gateway
        .receive(f.message("delegate", "delegate"))
        .await
        .unwrap();
    waiting_review(&f).await;
    f.adapter.attempts.lock().clear();
    f.adapter.fail_next.store(2, Ordering::SeqCst);
    f.script.hold_review.store(false, Ordering::SeqCst);
    f.script.review_release.notify_one();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            f.c().gateway.dispatch().await.unwrap();
            if !f.adapter.attempts.lock().is_empty()
                && f.c().runtime.status().active_sessions.is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(f.adapter.attempts.lock().len(), 1);
    assert!(!f.workspaces[0].path().join("agent.txt").exists());
    assert_eq!(f.submissions[0].load(Ordering::SeqCst), 0);
    f.restart().await;
    for _ in 0..3 {
        f.c().gateway.dispatch().await.unwrap();
    }
    assert_eq!(
        f.adapter.attempts.lock().len(),
        1,
        "unknown notification must retain its original delivery identity"
    );
    assert_eq!(
        f.script.review_requests.lock().len(),
        1,
        "unknown notification must not trigger another review/send"
    );
    assert_eq!(f.submissions[0].load(Ordering::SeqCst), 0);
    f.finish().await;
}

#[tokio::test]
async fn restart_during_review_never_reuses_the_old_decision_or_executes_the_old_call() {
    let mut f = Fixture::new_for(
        vec![
            call(
                "delegate_task",
                json!({"goal":"write after review"}),
                "delegate",
            ),
            answer("delegated"),
            tool_write(),
        ],
        true,
    )
    .await;
    f.script.hold_review.store(true, Ordering::SeqCst);
    let parent = f
        .c()
        .gateway
        .open_browser_workspace(&f.c().browser, Uuid::new_v4(), f.devices[0].id, None)
        .await
        .unwrap();
    let turn = f
        .c()
        .gateway
        .submit_browser_task(&f.c().browser, parent, Uuid::new_v4(), "delegate".into())
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    waiting_review(&f).await;
    assert_eq!(f.submissions[0].load(Ordering::SeqCst), 0);
    f.restart().await;
    f.script.hold_review.store(false, Ordering::SeqCst);
    f.script.review_release.notify_waiters();
    f.script.replies.lock().extend([
        tool_write(),
        answer("fresh request completed"),
        answer("result confirmed"),
    ]);
    // The first interrupted review uses a bounded external wait. Explicitly
    // resume with the current task revision instead of sleeping for its timer.
    f.script.replies.lock().push_front(answer("inspected"));
    let receipt = delegate_receipt(&f.script.requests.lock()[1]);
    f.script.replies.lock().push_front(call(
        "inspect_task",
        json!({"task_id":receipt["task_id"]}),
        "inspect-restart",
    ));
    let inspect = f
        .c()
        .gateway
        .submit_browser_task(&f.c().browser, parent, Uuid::new_v4(), "inspect".into())
        .await
        .unwrap();
    f.settled(parent, inspect.turn_id).await;
    let status = tool_value(&f.script.requests.lock()[4], "state");
    f.script.replies.lock().push_front(answer("resuming"));
    f.script.replies.lock().push_front(call("control_task",json!({"task_id":receipt["task_id"],"expected_revision":status["revision"],"action":"resume"}),"resume-review"));
    let resume = f
        .c()
        .gateway
        .submit_browser_task(&f.c().browser, parent, Uuid::new_v4(), "resume".into())
        .await
        .unwrap();
    f.settled(parent, resume.turn_id).await;
    pump(&f, 10).await;
    assert_eq!(f.submissions[0].load(Ordering::SeqCst), 1);
    assert_eq!(
        f.script.review_requests.lock().len(),
        2,
        "old review cannot approve a new turn"
    );
    f.finish().await;
}

#[tokio::test]
async fn main_reviewer_rejection_is_enforced_by_the_original_permission_middleware() {
    let f = Fixture::new_for(
        vec![
            call(
                "delegate_task",
                json!({"goal":"inspect only; do not modify files"}),
                "delegate",
            ),
            answer("delegated"),
            tool_write(),
            answer("write denied; inspection only"),
            answer("result"),
        ],
        true,
    )
    .await;
    f.script.review_replies.lock().push_back(answer(&json!({"approve":false,"deletion_possible":true,"reason":"write conflicts with the user requirement","notice":""}).to_string()));
    let parent = f
        .c()
        .gateway
        .open_browser_workspace(&f.c().browser, Uuid::new_v4(), f.devices[0].id, None)
        .await
        .unwrap();
    let turn = f
        .c()
        .gateway
        .submit_browser_task(&f.c().browser, parent, Uuid::new_v4(), "delegate".into())
        .await
        .unwrap();
    f.settled(parent, turn.turn_id).await;
    pump(&f, 5).await;
    assert_eq!(f.submissions[0].load(Ordering::SeqCst), 0);
    assert!(!f.workspaces[0].path().join("agent.txt").exists());
    assert!(f.script.requests.lock()[3]["messages"]
        .to_string()
        .contains("write conflicts with the user requirement"));
    assert!(f
        .c()
        .gateway
        .browser_interactions(&f.c().browser)
        .await
        .unwrap()
        .is_empty());
    f.finish().await;
}
