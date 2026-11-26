/// 同一任务两次终态回填（投递重试）：结果必须再投，工具调用只计一次。
///
/// sink 侧接线与 `WorkflowRuntime::new` 一致：按 taskId 观察结果中的 `tool_count`，
/// 限额判定归 `RunLimitState`。
#[test]
fn duplicate_settle_redelivers_result_and_counts_tool_calls_once() {
    let (table, _rx) = make_table();
    let limit = Arc::new(crate::runner::RunLimitState::new(Some(5), None));
    let delivered = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_deliveries = Arc::clone(&delivered);
    let sink_limit = Arc::clone(&limit);
    table.set_terminal_sink(Arc::new(move |record| {
        if let Some(count) = record
            .result
            .as_ref()
            .and_then(|result| result.tool_count())
        {
            sink_limit.observe_tool_calls(
                &record.run_id,
                &record.task_id,
                record.agent_id.unwrap_or_default(),
                count,
            );
        }
        sink_deliveries
            .lock()
            .expect("sink deliveries lock mutex must remain unpoisoned throughout the test")
            .push(record.clone());
    }));

    let result = AgentRunResult::Ok {
        output: "done".into(),
        usage: Usage { output_tokens: 3 },
        model: None,
        tool_count: Some(3),
        token_count: None,
        phase: None,
        duration_ms: None,
    };
    let _ = table.publish("t-1".into(), make_params("run-1", 1));
    assert!(matches!(
        table.mark_executing("run-1", "t-1", CancellationToken::new()),
        ClaimOutcome::Claimed(_)
    ));
    for _ in 0..2 {
        let _ = table.settle(
            "run-1",
            "t-1",
            SettleAdmission::ExecutingOnly,
            WorkflowTaskState::Completed,
            Some(result.clone()),
            None,
        );
    }

    let delivered = delivered
        .lock()
        .expect("delivered lock mutex must remain unpoisoned throughout the test");
    assert_eq!(delivered.len(), 2, "重复回填必须再次投递终态结果");
    assert_eq!(delivered[0].task_id, delivered[1].task_id);
    assert_eq!(delivered[0].result, delivered[1].result, "重投同一份结果");
    drop(delivered);
    assert_eq!(
        limit.breach(),
        None,
        "同一任务只计首次：3+3 若重复累计会误报 maxToolCalls(5)"
    );

    // 另一任务继续累计并超限，证明首次的 3 确实计入、去重只按 task 生效。
    let _ = table.publish("t-2".into(), make_params("run-1", 2));
    assert!(matches!(
        table.mark_executing("run-1", "t-2", CancellationToken::new()),
        ClaimOutcome::Claimed(_)
    ));
    let _ = table.settle(
        "run-1",
        "t-2",
        SettleAdmission::ExecutingOnly,
        WorkflowTaskState::Completed,
        Some(result),
        None,
    );
    assert_eq!(
        limit.breach().as_deref(),
        Some("workflow exceeded maxToolCalls (5)"),
        "另一任务的 3 次调用应正常累计到 6"
    );
}

fn scope_close_failure() -> AgentRunResult {
    AgentRunResult::Dead {
        scope_close_incomplete: false,
        reason: Some("event-forwarder-failed".into()),
        detail: Some("Original failure with outstanding writers".into()),
        duration_ms: None,
        token_count: None,
    }
    .after_scope_close(false)
}

#[test]
fn committed_scope_failure_fences_only_its_run_and_cancels_peers() {
    let (table, _rx) = make_table();
    for (task, agent) in [("failed", 1), ("queued", 2), ("running", 3)] {
        let _ = table.publish(task.into(), make_params("run-1", agent));
    }
    let failed_token = CancellationToken::new();
    let peer_token = CancellationToken::new();
    assert!(matches!(
        table.mark_executing("run-1", "failed", failed_token),
        ClaimOutcome::Claimed(_)
    ));
    assert!(matches!(
        table.mark_executing("run-1", "running", peer_token.clone()),
        ClaimOutcome::Claimed(_)
    ));
    let _ = table.publish("other".into(), make_params("run-2", 1));

    let failure = scope_close_failure();
    match table.settle(
        "run-1",
        "failed",
        SettleAdmission::ExecutingOnly,
        WorkflowTaskState::Failed,
        Some(failure.clone()),
        None,
    ) {
        SettleOutcome::Settled(record) => {
            assert_eq!(record.state, WorkflowTaskState::Failed);
            assert_eq!(record.result, Some(failure));
        }
        other => panic!("scope failure must commit: {other:?}"),
    }
    assert!(peer_token.is_cancelled(), "active same-run peer must stop");
    for task in ["queued", "running"] {
        assert_eq!(
            table.get("run-1", task).unwrap().state,
            WorkflowTaskState::Cancelled
        );
        assert!(matches!(
            table.mark_executing("run-1", task, CancellationToken::new()),
            ClaimOutcome::AlreadyTerminal(_)
        ));
        assert!(table
            .claim_for_consumer("run-1", task, "peer-claim", None)
            .is_none());
    }

    match table.publish("retry".into(), make_params("run-1", 4)) {
        PublishOutcome::Published(record) => assert_eq!(record.state, WorkflowTaskState::Cancelled),
        other => panic!("retry should have a terminal receipt: {other:?}"),
    }
    assert!(matches!(
        table.mark_executing("run-1", "retry", CancellationToken::new()),
        ClaimOutcome::AlreadyTerminal(_)
    ));
    assert!(table
        .claim_for_consumer("run-1", "retry", "retry-claim", None)
        .is_none());
    assert!(table
        .claim_for_consumer("run-2", "other", "other-claim", None)
        .is_some());
    let _ = table.publish("other-late".into(), make_params("run-2", 2));
    assert!(matches!(
        table.mark_executing("run-2", "other-late", CancellationToken::new()),
        ClaimOutcome::Claimed(_)
    ));
}

#[test]
fn conflicting_late_scope_failure_does_not_fence_a_successful_run() {
    let (table, _rx) = make_table();
    let _ = table.publish("completed".into(), make_params("run-1", 1));
    assert!(matches!(
        table.mark_executing("run-1", "completed", CancellationToken::new()),
        ClaimOutcome::Claimed(_)
    ));
    let success = ok_result();
    assert!(matches!(
        table.settle(
            "run-1",
            "completed",
            SettleAdmission::ExecutingOnly,
            WorkflowTaskState::Completed,
            Some(success.clone()),
            None
        ),
        SettleOutcome::Settled(_)
    ));
    for _ in 0..2 {
        match table.settle(
            "run-1",
            "completed",
            SettleAdmission::ExecutingOnly,
            WorkflowTaskState::Failed,
            Some(scope_close_failure()),
            None,
        ) {
            SettleOutcome::AlreadyTerminal(record) => {
                assert_eq!(record.state, WorkflowTaskState::Completed);
                assert_eq!(record.result, Some(success.clone()));
            }
            other => panic!("first terminal result must win: {other:?}"),
        }
    }
    let _ = table.publish("next".into(), make_params("run-1", 2));
    assert!(table
        .claim_for_consumer("run-1", "next", "next-claim", None)
        .is_some());
}

#[test]
fn nonadmitted_scope_failure_does_not_fence_pending_work() {
    let (table, _rx) = make_table();
    let _ = table.publish("unclaimed".into(), make_params("run-1", 1));
    assert!(matches!(
        table.settle(
            "run-1",
            "unclaimed",
            SettleAdmission::ExecutingOnly,
            WorkflowTaskState::Failed,
            Some(scope_close_failure()),
            None
        ),
        SettleOutcome::NotAdmitted(_)
    ));
    assert_eq!(
        table.get("run-1", "unclaimed").unwrap().state,
        WorkflowTaskState::Published
    );
    assert!(matches!(
        table.mark_executing("run-1", "unclaimed", CancellationToken::new()),
        ClaimOutcome::Claimed(_)
    ));
    let _ = table.publish("next".into(), make_params("run-1", 2));
    assert!(table
        .claim_for_consumer("run-1", "next", "next-claim", None)
        .is_some());
}

// ─── tool 任务（issue #407）：同表、kind 投影、容量与 kill 栅栏 ───────────────

fn publish_tool_task(table: &WorkflowTaskTable, task_id: &str, run_id: &str, tool: &str) {
    let outcome = table.publish_tool(
        task_id.to_string(),
        run_id,
        tool.to_string(),
        serde_json::Map::new(),
        0,
        0,
    );
    assert!(
        matches!(outcome, PublishOutcome::Published(_)),
        "tool 任务首个发布必须提交：{outcome:?}"
    );
}

#[test]
fn tool_tasks_share_the_table_without_consuming_agent_capacity() {
    let (table, _rx) = make_table();
    let _ = table.publish("a-1".into(), make_params("run-1", 1));
    publish_tool_task(&table, "t-1", "run-1", "Read");

    let tool = table
        .get("run-1", "t-1")
        .expect("tool 任务必须可在同一张表寻址");
    assert_eq!(tool.kind(), crate::runtime::WorkflowTaskKind::Tool);
    assert!(tool.agent_id.is_none(), "tool 任务没有 agent 槽位");
    assert_eq!(
        tool.params
            .tool_call()
            .map(|(name, arguments)| (name, arguments.len())),
        Some(("Read", 0))
    );

    // 发布顺序、活跃计数与容量口径：maxAgents / maxConcurrency 只计 agent 槽位。
    assert_eq!(table.active_count_for_run("run-1"), 1);
    assert!(matches!(
        table.mark_executing("run-1", "a-1", CancellationToken::new()),
        ClaimOutcome::Claimed(_)
    ));
    assert_eq!(table.executing_count_for_run("run-1"), 1);
    assert_eq!(table.executing_tool_count_for_run("run-1"), 0);

    // agent 容量已满（maxConcurrency = 1）：tool 仍可 claim（不受并发闸约束）。
    let claimed = table
        .claim_for_consumer("run-1", "t-1", "claim-tool", Some(1))
        .expect("tool 任务不得被 agent 并发上限拒绝");
    assert_eq!(claimed.kind(), crate::runtime::WorkflowTaskKind::Tool);
    assert_eq!(table.executing_tool_count_for_run("run-1"), 1);

    // 对照：第二个 agent 任务在容量已满时留在 published。
    let _ = table.publish("a-2".into(), make_params("run-1", 2));
    assert!(
        table
            .claim_for_consumer("run-1", "a-2", "claim-agent-2", Some(1))
            .is_none(),
        "maxConcurrency 仍必须约束 agent 槽位"
    );

    // 序列化投影：kind 在 payload tag 上，tool 任务不带 agentId。
    let value = serde_json::to_value(&tool).expect("任务记录可序列化");
    assert_eq!(value["params"]["kind"], "tool");
    assert_eq!(value["params"]["toolSeq"], 0);
    assert_eq!(value["params"]["agentSeq"], 0);
    assert!(value.get("agentId").is_none());
    assert_eq!(
        table
            .events()
            .page(None, 10)
            .entries
            .iter()
            .find(|event| event.task_id == "t-1")
            .and_then(|event| event.agent_id),
        None,
        "tool 任务事件不携带 agent 身份"
    );
}

#[test]
fn tool_task_terminal_outcome_is_single_and_idempotent() {
    let (table, _rx) = make_table();
    publish_tool_task(&table, "t-1", "run-1", "Read");
    assert!(matches!(
        table.mark_executing("run-1", "t-1", CancellationToken::new()),
        ClaimOutcome::Claimed(_)
    ));

    let first = crate::protocol::WorkflowToolOutcome::Ok {
        structured_content: Some(serde_json::json!({"answer": 42})),
        content: "42".into(),
    };
    match table.settle_tool(
        "run-1",
        "t-1",
        SettleAdmission::ExecutingOnly,
        WorkflowTaskState::Completed,
        Some(first.clone()),
        None,
    ) {
        SettleOutcome::Settled(record) => {
            assert_eq!(record.state, WorkflowTaskState::Completed);
            assert_eq!(record.tool_outcome, Some(first.clone()));
            assert!(record.result.is_none(), "tool 任务不写 agent 结果字段");
        }
        other => panic!("首次 tool 终态必须提交：{other:?}"),
    }

    // 不同结果的迟到回填：保留首次（幂等）；不会改写 agent 字段。
    match table.settle_tool(
        "run-1",
        "t-1",
        SettleAdmission::ExecutingOnly,
        WorkflowTaskState::Failed,
        Some(crate::protocol::WorkflowToolOutcome::error(
            crate::protocol::WorkflowToolErrorCode::OutcomeUnknown,
            "Read",
            "late",
        )),
        Some("late".into()),
    ) {
        SettleOutcome::AlreadyTerminal(record) => {
            assert_eq!(record.state, WorkflowTaskState::Completed);
            assert_eq!(record.tool_outcome, Some(first));
        }
        other => panic!("重复回填必须幂等：{other:?}"),
    }
    assert_eq!(
        table.get("run-1", "t-1").unwrap().state,
        WorkflowTaskState::Completed
    );
}

#[test]
fn agent_kill_does_not_cancel_tool_tasks_but_run_kill_does() {
    let (table, _rx) = make_table();
    let _ = table.publish("a-1".into(), make_params("run-1", 3));
    publish_tool_task(&table, "t-1", "run-1", "Read");

    let cancelled = table.cancel_agent("run-1", 3, Some("agent killed".into()));
    assert_eq!(cancelled, vec!["a-1".to_string()]);
    assert_eq!(
        table.get("run-1", "t-1").unwrap().state,
        WorkflowTaskState::Published,
        "agent 级 kill 不命中 tool 任务（它们不是该 agent 的槽位）"
    );

    let cancelled = table.cancel_run("run-1", Some("run killed".into()));
    assert_eq!(cancelled, vec!["t-1".to_string()]);
    assert_eq!(
        table.get("run-1", "t-1").unwrap().state,
        WorkflowTaskState::Cancelled
    );
    // 栅栏覆盖迟到发布：run kill 之后的 tool 发布直接终态。
    match table.publish_tool(
        "t-2".to_string(),
        "run-1",
        "Bash".to_string(),
        serde_json::Map::new(),
        1,
        0,
    ) {
        PublishOutcome::Published(record) => {
            assert_eq!(record.state, WorkflowTaskState::Cancelled);
        }
        other => panic!("kill 栅栏必须作用于 tool 发布：{other:?}"),
    }
}

#[test]
fn cancelling_an_executing_tool_task_marks_unknown_side_effects() {
    let (table, _rx) = make_table();
    publish_tool_task(&table, "t-queued", "run-1", "Read");
    publish_tool_task(&table, "t-live", "run-2", "Bash");
    assert!(matches!(
        table.mark_executing("run-2", "t-live", CancellationToken::new()),
        ClaimOutcome::Claimed(_)
    ));

    // 排队中的 tool（未 claim）被取消：没有已 dispatch 的调用，不产生未知副作用。
    let _ = table.cancel_run("run-1", Some("workflow run killed".into()));
    assert!(!table.run_unknown_side_effects("run-1"));

    // 执行中的 tool 被取消（kill / run 终止）：调用可能已 dispatch ⇒ 未知副作用。
    let _ = table.cancel_run("run-2", Some("workflow run killed".into()));
    assert!(
        table.run_unknown_side_effects("run-2"),
        "执行中的 tool 任务被取消必须标记未知副作用"
    );
    assert_eq!(
        table.get("run-2", "t-live").unwrap().state,
        WorkflowTaskState::Cancelled
    );

    // 退役清理：未知副作用标记随 run 回收（不跨 run 泄漏）。
    table.forget_run_fences("run-2");
    assert!(!table.run_unknown_side_effects("run-2"));
}
