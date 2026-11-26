#[test]
fn test_agent_task_params_reject_cross_run_identity() {
    let result = crate::runner::run_protocol::parse_agent_task_params(
        Some(serde_json::json!({
            "runId": "other-run",
            "taskId": "t-1",
            "agentId": 7,
            "prompt": "inspect"
        })),
        "run-1",
    );

    assert_eq!(
        result.unwrap_err(),
        "runId does not match the active workflow run"
    );
}

#[test]
fn test_run_scoped_done_rejects_cross_run_identity() {
    let result = crate::runner::run_protocol::parse_run_scoped::<WorkflowDoneParams>(
        Some(serde_json::json!({
            "runId": "other-run",
            "status": "completed"
        })),
        "run-1",
    );

    assert_eq!(
        result.unwrap_err(),
        "runId does not match the active workflow run"
    );
}

#[test]
fn test_run_scoped_journal_rejects_missing_run_identity() {
    let result = crate::runner::run_protocol::parse_run_scoped::<JournalTruncateParams>(
        Some(serde_json::json!({})),
        "run-1",
    );

    assert_eq!(result.err(), Some("invalid run-scoped RPC parameters"));
}

#[test]
fn test_start_ack_accepts_matching_protocol_and_build() {
    crate::runner::run_protocol::validate_start_ack(serde_json::json!({
        "ok": true,
        "protocolVersion": crate::runner::artifact::WORKFLOW_PROTOCOL_VERSION,
        "buildId": crate::runner::artifact::WORKFLOW_BUILD_ID
    }))
    .expect("the validate start ack fixture result is required for this assertion");
}

#[test]
fn test_start_ack_rejects_protocol_mismatch() {
    let error = crate::runner::run_protocol::validate_start_ack(serde_json::json!({
        "ok": true,
        "protocolVersion": crate::runner::artifact::WORKFLOW_PROTOCOL_VERSION + 1,
        "buildId": crate::runner::artifact::WORKFLOW_BUILD_ID
    }))
    .unwrap_err();

    assert!(error.to_string().contains("protocol mismatch"));
}

/// [回归测试] kill 后 run 必须收敛为 Killed 且写 completed_at（不能停在幽灵 Running）。
///
/// 只依赖真实引擎启动（kill 分支不经过 agent 执行），因此可在本 crate 覆盖。
#[tokio::test]
#[serial]
async fn test_kill_marks_run_killed_in_progress_store() {
    if !require_explicit_engine() {
        return;
    }
    let tmp =
        tempfile::TempDir::new().expect("the new fixture result is required for this assertion");
    let cwd = tmp
        .path()
        .to_str()
        .expect("the fixture text must be valid UTF-8");

    let executor = Arc::new(MockAgentExecutor {
        delay: std::time::Duration::from_secs(5),
    }) as Arc<dyn AgentExecutor>;
    let runner = WorkflowRunner::new(executor, cwd, None);
    let journal = Arc::new(WorkflowJournalStore::new(cwd));
    let progress = Arc::new(WorkflowProgressStore::new());
    let (done_tx, mut done_rx) = tokio::sync::watch::channel(None);
    // 持有 kill_tx：v1 打通后 kill 通道是 (kill_tx → kill_rx)，测试需保留 sender 以便触发
    let (kill_tx, kill_rx) = tokio::sync::oneshot::channel();

    let script = r#"
export const meta = { name: 'kill-test', description: 'kill test' }
const result = await agent('say hello')
return { output: result }
"#;

    let input = WorkflowInput {
        script: script.to_string(),
        args: None,
        max_concurrency: 3,
        budget_total: None,
        limits: WorkflowLimits::default(),
        workflow_name: "kill-test".to_string(),
        resume_from: None,
        write_intent: None,
        git_observation: Default::default(),
    };

    let run_id = uuid::Uuid::now_v7().to_string();
    let progress_for_runner = Arc::clone(&progress);
    let run_id_for_runner = run_id.clone();
    let run_handle = tokio::spawn(async move {
        runner
            .run(
                run_id_for_runner,
                input,
                progress_for_runner,
                journal,
                done_tx,
                kill_rx,
            )
            .await
    });

    // 等待 run_started 写入 progress_store（run 进入 Running 状态）
    let progress_wait = Arc::clone(&progress);
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            if let Some(run) = progress_wait.run_snapshot(&run_id) {
                if matches!(run.status, RunStatus::Running) {
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("run 未在超时内进入 Running 状态");

    // 触发 kill（等效 workflow/kill_run RPC → WorkflowTaskRegistry::kill → kill_tx）
    kill_tx
        .send(())
        .expect("kill tx send test receiver must remain available for this delivery");

    // 等待完成信号：kill 分支是 done_tx 的唯一出口，必达
    tokio::time::timeout(std::time::Duration::from_secs(30), done_rx.changed())
        .await
        .expect("kill 后未收到完成信号")
        .expect("the expect fixture result is required for this assertion");
    let result = done_rx
        .borrow()
        .clone()
        .expect("the clone fixture result is required for this assertion");
    assert_eq!(result.status, "killed");

    // 核心断言：kill 后 progress_store 显示 Killed（workflow/list_runs 与 run_snapshot 同源，
    // 回归点：修复前此处永久 Running —— 幽灵 running 根因）
    let run = progress
        .run_snapshot(&run_id)
        .expect("run 应存在于 progress_store");
    assert!(
        matches!(run.status, RunStatus::Killed),
        "kill 后 progress_store 应显示 Killed，实际 {:?}",
        run.status
    );
    assert!(
        run.completed_at.is_some(),
        "Killed 条目必须设置 completed_at，否则 cleanup_completed 永不清理"
    );

    // run() 应在 kill 分支后正常返回 Ok
    run_handle.await.expect("the run handle fixture result is required for this assertion").expect("kill marks run killed in progress store: the completed fixture operation must provide its result payload");
}

/// [回归测试] Node 自然崩溃（非 kill）时 msg_loop failed 收尾必须收敛 progress_store
/// 为 Failed（issue 2026-08-05 遗留 2：修复前 run 永久 Running，幽灵 running 与
/// kill 分支同源）。
///
/// 防假阳性：脚本先发布 `agent/task`（**不等待结果**，故本用例不需要宿主消费者：
/// P6 起 agent 执行权在宿主，等待结果会永久挂起），随后顶层 throw → Node 进程在
/// workflow/start 成功后自然退出；崩溃时进程已死，没有机会发 run_done progress
/// 事件，只有 msg_loop 收尾（stdout 关闭 → recv None → final_result 保持 "failed"）
/// 能标记终态。修复前该路径不写 progress_store，断言必然失败。
#[tokio::test]
#[serial]
async fn test_natural_crash_marks_run_failed_in_progress_store() {
    if !require_explicit_engine() {
        return;
    }
    let tmp =
        tempfile::TempDir::new().expect("the new fixture result is required for this assertion");
    let cwd = tmp
        .path()
        .to_str()
        .expect("the fixture text must be valid UTF-8");

    let executor = Arc::new(MockAgentExecutor {
        delay: std::time::Duration::ZERO,
    }) as Arc<dyn AgentExecutor>;
    let runner = WorkflowRunner::new(executor, cwd, None);
    let journal = Arc::new(WorkflowJournalStore::new(cwd));
    let progress = Arc::new(WorkflowProgressStore::new());
    let (done_tx, _done_rx) = tokio::sync::watch::channel(None);
    let (_kill_tx, kill_rx) = tokio::sync::oneshot::channel();

    let script = r#"
export const meta = { name: 'crash-test', description: 'crash test' }
agent('say hello') // 发布 agent/task（不等待结果：本用例不依赖宿主消费者）
throw new Error('intentional crash after agent')
"#;

    let input = WorkflowInput {
        script: script.to_string(),
        args: None,
        max_concurrency: 3,
        budget_total: None,
        limits: WorkflowLimits::default(),
        workflow_name: "crash-test".to_string(),
        resume_from: None,
        write_intent: None,
        git_observation: Default::default(),
    };

    let run_id = uuid::Uuid::now_v7().to_string();
    let progress_for_runner = Arc::clone(&progress);
    let run_id_for_runner = run_id.clone();
    let run_handle = tokio::spawn(async move {
        runner
            .run(
                run_id_for_runner,
                input,
                progress_for_runner,
                journal,
                done_tx,
                kill_rx,
            )
            .await
    });

    // 等待 run_started 写入 progress_store（run 出现）——证明 workflow/start 已成功
    // 且 msg_loop 已 spawn（修复前此处之后永久 Running）。
    //
    // 注意：不能等待 Running 状态——mock executor 秒回 + 脚本顶层立即 throw，
    // Running 窗口仅毫秒级，50ms 轮询必然错过；run 可能直接以 Failed 终态出现
    // （崩溃收敛），因此只等待"run 存在"，终态由下方断言验证。
    let progress_wait = Arc::clone(&progress);
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            if progress_wait.run_snapshot(&run_id).is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("run 未在超时内出现");

    // 不触发 kill：Node 自然崩溃 → run() 应正常返回
    run_handle.await.expect("the run handle fixture result is required for this assertion").expect("natural crash marks run failed in progress store: the completed fixture operation must provide its result payload");

    // 核心断言：自然崩溃后 progress_store 显示 Failed（修复前此处永久 Running）
    let run = progress
        .run_snapshot(&run_id)
        .expect("run 应存在于 progress_store");
    assert!(
        matches!(run.status, RunStatus::Failed),
        "自然崩溃后 progress_store 应显示 Failed，实际 {:?}",
        run.status
    );
    assert!(
        run.completed_at.is_some(),
        "Failed 条目必须设置 completed_at，否则 cleanup_completed 永不清理"
    );
}

#[test]
fn fatal_host_failure_uses_the_existing_first_failure_slot_per_run() {
    let executor = Arc::new(MockAgentExecutor {
        delay: std::time::Duration::ZERO,
    }) as Arc<dyn AgentExecutor>;
    let runner = WorkflowRunner::new(executor, ".", None);
    let failed = Arc::new(RunLimitState::new(None, None));
    let independent = Arc::new(RunLimitState::new(None, None));
    runner
        .run_limit_states
        .insert("failed".into(), Arc::clone(&failed));
    runner
        .run_limit_states
        .insert("independent".into(), Arc::clone(&independent));
    runner.note_run_failure("failed", "scope not closed");
    runner.note_run_failure("failed", "later failure");
    assert_eq!(failed.breach().as_deref(), Some("scope not closed"));
    assert_eq!(independent.breach(), None);
    runner.note_run_failure("already retired", "late result");
    assert_eq!(runner.run_limit_states.len(), 2);
}

/// 结算路径按执行器解析的上限记录迭代耗尽；重投不重复记录。
struct FixedLimitsExecutor {
    limits: peri_acp_types::workflow::AgentExecutionLimits,
}

#[async_trait::async_trait]
impl AgentExecutor for FixedLimitsExecutor {
    async fn execute(&self, _params: AgentRunParams) -> AgentRunResult {
        AgentRunResult::Skipped
    }

    fn execution_limits(
        &self,
        _run_id: &str,
        _agent_id: u64,
    ) -> Option<peri_acp_types::workflow::AgentExecutionLimits> {
        Some(self.limits)
    }
}

#[test]
fn iteration_exhaustion_records_resolved_limit_and_usage() {
    let executor = Arc::new(FixedLimitsExecutor {
        limits: peri_acp_types::workflow::AgentExecutionLimits {
            iteration_limit: 400,
            iteration_limit_source: peri_acp_types::workflow::IterationLimitSource::AgentDefinition,
            iterations_used: Some(400),
        },
    }) as Arc<dyn AgentExecutor>;
    let runner = WorkflowRunner::new(executor, ".", None);
    runner
        .run_limit_states
        .insert("run-it".into(), Arc::new(RunLimitState::new(None, None)));
    let result = AgentRunResult::Dead {
        scope_close_incomplete: false,
        reason: Some(peri_acp_types::workflow::AGENT_DEAD_REASON_MAX_ITERATIONS.into()),
        detail: Some("Max iterations exceeded (400)".into()),
        duration_ms: Some(1),
        token_count: Some(75_323),
    };
    assert_eq!(
        runner
            .note_task_execution("run-it", "task-1", 9, &result)
            .as_deref(),
        Some("workflow exceeded maxIterations (400)")
    );
    // 投递重试不得重复记录同一条超限。
    assert_eq!(
        runner.note_task_execution("run-it", "task-1", 9, &result),
        None
    );
    let state = runner
        .run_limit_states
        .get("run-it")
        .expect("run limit state must remain registered");
    let breaches = state.breaches();
    assert_eq!(breaches.len(), 1, "重投只保留一条迭代超限记录");
    assert_eq!(
        breaches[0].scope,
        peri_acp_types::workflow::LimitScope::Iterations
    );
    assert_eq!(breaches[0].limit, Some(400), "上限按解析值记录");
    assert_eq!(breaches[0].observed, Some(400), "用量精确到实际迭代数");
    assert_eq!(
        breaches[0].attempt.as_ref().map(|attempt| attempt.agent_id),
        Some(9)
    );
    assert_eq!(
        state.observed_tokens(),
        Some(75_323),
        "dead 尝试上报的 token 用量同样累计"
    );
}

/// 正常完成不产生超限，但 token 用量照常累计（未配置预算也不例外）。
#[test]
fn successful_task_records_usage_without_breach() {
    let executor = Arc::new(MockAgentExecutor {
        delay: std::time::Duration::ZERO,
    }) as Arc<dyn AgentExecutor>;
    let runner = WorkflowRunner::new(executor, ".", None);
    runner.run_limit_states.insert(
        "run-ok".into(),
        Arc::new(RunLimitState::new(None, Some(60_000))),
    );
    let result = AgentRunResult::Ok {
        output: "ok".into(),
        usage: Usage { output_tokens: 10 },
        model: None,
        tool_count: Some(2),
        token_count: Some(1_200),
        phase: None,
        duration_ms: None,
    };
    assert_eq!(
        runner.note_task_execution("run-ok", "task-1", 1, &result),
        None
    );
    let state = runner
        .run_limit_states
        .get("run-ok")
        .expect("run limit state must remain registered");
    assert!(state.breaches().is_empty());
    assert_eq!(state.observed_tokens(), Some(1_200));
}
