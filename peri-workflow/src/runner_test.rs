// ─── E2E 集成测试（真实 Node 引擎；产物经 PERI_WORKFLOW_ARTIFACT_DIR 注入）───
//
// P6 起 agent 执行权归宿主消费者：需要 agent 结果才能收敛的用例在宿主层
// （`peri-acp::host::workflow_consumer_tests`）覆盖；本文件只保留不依赖消费者的
// 引擎生命周期用例（kill / 自然崩溃）。

use serial_test::serial;

use crate::journal::WorkflowJournalStore;
use crate::progress::{RunStatus, WorkflowProgressStore};
use crate::protocol::WorkflowDoneParams;
use crate::protocol::{AgentRunParams, AgentRunResult, Usage, WorkflowLimits};
use crate::runner::limits::RunLimitState;

use crate::runner::run_protocol::JournalTruncateParams;

use crate::runner::WorkflowInput;
use crate::runner::WorkflowResult;
use crate::runner::WorkflowRunner;
use peri_acp_types::workflow::AgentExecutor;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[test]
fn resume_reuses_only_the_contiguous_success_prefix() {
    let entries: Vec<crate::protocol::JournalEntry> = serde_json::from_value(serde_json::json!([
        {
            "key": "first",
            "seq": 0,
            "result": {"kind": "ok", "output": "one", "usage": {"outputTokens": 1}}
        },
        {
            "key": "failed",
            "seq": 1,
            "result": {"kind": "dead", "reason": "runagent-threw"}
        },
        {
            "key": "later",
            "seq": 2,
            "result": {"kind": "ok", "output": "must-replay", "usage": {"outputTokens": 1}}
        }
    ]))
    .expect("from_value must accept this test fixture representation");

    let prefix = crate::runner::run_protocol::reusable_journal_prefix(entries);
    assert_eq!(prefix.len(), 1);
    assert_eq!(prefix[0].key, "first");
}

#[test]
fn resume_skipped_entry_is_a_cache_boundary() {
    let entries: Vec<crate::protocol::JournalEntry> = serde_json::from_value(serde_json::json!([
        {
            "key": "first",
            "seq": 0,
            "result": {"kind": "skipped"}
        },
        {
            "key": "later",
            "seq": 1,
            "result": {"kind": "ok", "output": "must-replay", "usage": {"outputTokens": 1}}
        }
    ]))
    .expect("from_value must accept this test fixture representation");

    assert!(crate::runner::run_protocol::reusable_journal_prefix(entries).is_empty());
}

#[test]
fn resume_gap_is_not_treated_as_a_success_prefix() {
    let entries: Vec<crate::protocol::JournalEntry> = serde_json::from_value(serde_json::json!([
        {
            "key": "later",
            "seq": 2,
            "result": {"kind": "ok", "output": "must-replay", "usage": {"outputTokens": 1}}
        }
    ]))
    .expect("from_value must accept this test fixture representation");

    assert!(crate::runner::run_protocol::reusable_journal_prefix(entries).is_empty());
}

fn tool_journal_entry(tool_seq: u64, agent_seq: u64) -> crate::protocol::ToolJournalEntry {
    crate::protocol::ToolJournalEntry {
        tool_seq,
        agent_seq,
        task_id: format!("tool-{tool_seq}"),
        tool_name: "Read".into(),
        arguments: serde_json::Map::new(),
        outcome: peri_acp_types::workflow::WorkflowToolOutcome::Ok {
            structured_content: None,
            content: "ok".into(),
        },
    }
}

#[test]
fn resume_tool_prefix_keeps_a_contiguous_agent_aligned_prefix() {
    let entries = vec![
        tool_journal_entry(0, 0),
        tool_journal_entry(1, 1),
        tool_journal_entry(2, 1),
    ];

    let prefix = crate::runner::run_protocol::replayable_tool_prefix(entries, 1);

    assert_eq!(
        prefix.len(),
        3,
        "toolSeq 连续且 agentSeq 不超界时必须全部保留"
    );
    assert_eq!(prefix[2].tool_seq, 2);
}

#[test]
fn resume_tool_prefix_stops_at_a_tool_seq_gap() {
    let entries = vec![
        tool_journal_entry(0, 0),
        tool_journal_entry(2, 0),
        tool_journal_entry(3, 0),
    ];

    let prefix = crate::runner::run_protocol::replayable_tool_prefix(entries, 5);

    assert_eq!(prefix.len(), 1, "首个 toolSeq 断裂即停止");
    assert_eq!(prefix[0].tool_seq, 0);
}

#[test]
fn resume_tool_prefix_stops_when_agent_seq_exceeds_the_agent_prefix() {
    let entries = vec![
        tool_journal_entry(0, 0),
        tool_journal_entry(1, 2),
        tool_journal_entry(2, 2),
    ];

    let prefix = crate::runner::run_protocol::replayable_tool_prefix(entries, 1);

    assert_eq!(prefix.len(), 1, "agentSeq 超出可回放 agent 前缀即停止");
    assert_eq!(prefix[0].task_id, "tool-0");
}

#[test]
fn resume_tool_prefix_requires_the_file_to_start_at_zero_and_in_order() {
    let entries = vec![tool_journal_entry(1, 0), tool_journal_entry(2, 0)];

    assert!(
        crate::runner::run_protocol::replayable_tool_prefix(entries, 9).is_empty(),
        "前缀必须从 toolSeq 0 开始（按文件顺序，不排序）"
    );
}

struct CountingAgentExecutor {
    calls: Arc<AtomicU64>,
}

#[async_trait::async_trait]
impl AgentExecutor for CountingAgentExecutor {
    async fn execute(&self, _params: AgentRunParams) -> AgentRunResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        AgentRunResult::Dead {
            scope_close_incomplete: false,
            reason: Some("test executor must not run".into()),
            detail: None,
            duration_ms: None,
            token_count: None,
        }
    }
}

#[tokio::test]
async fn resume_sequence_gap_fails_before_any_agent_execution() {
    let tmp =
        tempfile::TempDir::new().expect("the new fixture result is required for this assertion");
    let cwd = tmp
        .path()
        .to_str()
        .expect("the fixture text must be valid UTF-8");
    let journal = Arc::new(WorkflowJournalStore::new(cwd));
    journal
        .init_run("source", "script")
        .expect("the init run fixture result is required for this assertion");
    std::fs::write(
        journal.run_dir("source").join("journal.jsonl"),
        "{\"key\":\"later\",\"seq\":1,\"result\":{\"kind\":\"ok\",\"output\":\"later\",\"usage\":{\"outputTokens\":1}}}\n",
    )
    .expect("the test fixture file must be writable");

    let calls = Arc::new(AtomicU64::new(0));
    let executor = Arc::new(CountingAgentExecutor {
        calls: Arc::clone(&calls),
    });
    let runner = WorkflowRunner::new(executor, cwd, None);
    let progress = Arc::new(WorkflowProgressStore::new());
    let (done_tx, _done_rx) = tokio::sync::watch::channel(None);
    let (_kill_tx, kill_rx) = tokio::sync::oneshot::channel();
    let input = WorkflowInput {
        script: "export const meta = { name: 'gap', description: 'gap' }".into(),
        args: None,
        max_concurrency: 3,
        budget_total: None,
        limits: WorkflowLimits::default(),
        workflow_name: "gap".into(),
        resume_from: Some("source".into()),
        write_intent: None,
        git_observation: Default::default(),
    };

    let error = runner
        .run("target".into(), input, progress, journal, done_tx, kill_rx)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("expected seq 0"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

/// Mock executor: 返回固定结果（delay 用于模拟慢 agent，保证 kill 测试的窗口）
struct MockAgentExecutor {
    delay: std::time::Duration,
}

#[async_trait::async_trait]
impl AgentExecutor for MockAgentExecutor {
    async fn execute(&self, params: AgentRunParams) -> AgentRunResult {
        tokio::time::sleep(self.delay).await;
        let preview = &params.prompt[..20.min(params.prompt.len())];
        AgentRunResult::Ok {
            output: format!("mock response to: {preview}").into(),
            usage: Usage { output_tokens: 10 },
            model: None,
            tool_count: None,
            token_count: None,
            phase: None,
            duration_ms: None,
        }
    }
}

#[test]
fn run_limit_state_tracks_tool_calls_and_keeps_first_breach() {
    let state = RunLimitState::new(Some(5), None);
    assert!(state.observe_tool_calls("run-1", "t-1", 0, 3).is_none());
    assert_eq!(state.breach(), None, "未超限不得置 breach");
    assert_eq!(
        state.observe_tool_calls("run-1", "t-2", 0, 3).as_deref(),
        Some("workflow exceeded maxToolCalls (5)"),
        "累计超限时返回首次 breach 详情",
    );
    assert_eq!(
        state.breach().as_deref(),
        Some("workflow exceeded maxToolCalls (5)"),
    );
    // breach 只写首次：后到的超限文本不得覆写（沿用旧 limit_breach 语义）。
    state.set_breach("workflow exceeded maxAgents (0)".into());
    assert_eq!(
        state.breach().as_deref(),
        Some("workflow exceeded maxToolCalls (5)"),
    );
}

#[test]
fn run_limit_state_counts_each_task_once_on_redelivery() {
    let state = RunLimitState::new(Some(5), None);
    assert!(
        state.observe_tool_calls("run-1", "t-1", 0, 3).is_none(),
        "首次终态结果正常计数"
    );
    assert!(
        state.observe_tool_calls("run-1", "t-1", 0, 3).is_none(),
        "同一 task 的投递重试不得重复计入（3+3 > 5 会误报超限）"
    );
    assert_eq!(state.breach(), None, "重投不产生 breach");
    assert_eq!(
        state.observe_tool_calls("run-1", "t-2", 0, 3).as_deref(),
        Some("workflow exceeded maxToolCalls (5)"),
        "去重只按 task 生效：首次的 3 确实计入，另一任务累计后超限",
    );
}

#[test]
fn concurrent_redelivery_of_one_task_counts_once() {
    let state = Arc::new(RunLimitState::new(Some(5), None));
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let state = Arc::clone(&state);
            scope.spawn(move || {
                assert!(
                    state.observe_tool_calls("run-1", "t-1", 0, 3).is_none(),
                    "并发重投不得触发超限"
                );
            });
        }
    });
    assert_eq!(state.breach(), None, "8 次并发重投也只能计一次");
    assert_eq!(
        state.observe_tool_calls("run-1", "t-2", 0, 3).as_deref(),
        Some("workflow exceeded maxToolCalls (5)"),
        "并发去重后累计值仍只来自首次的 3",
    );
}

fn tool_only_result(tool_count: u64) -> AgentRunResult {
    AgentRunResult::Ok {
        output: "ok".into(),
        usage: Usage { output_tokens: 0 },
        model: None,
        tool_count: Some(tool_count),
        token_count: None,
        phase: None,
        duration_ms: None,
    }
}

#[test]
fn duplicate_task_backfill_counts_tool_calls_once() {
    let executor = Arc::new(MockAgentExecutor {
        delay: std::time::Duration::ZERO,
    }) as Arc<dyn AgentExecutor>;
    let runner = WorkflowRunner::new(executor, ".", None);
    runner.run_limit_states.insert(
        "run-201".into(),
        Arc::new(RunLimitState::new(Some(5), None)),
    );

    // 首次终态提交：3 次工具调用正常计入。
    assert_eq!(
        runner.note_task_execution("run-201", "t-1", 0, &tool_only_result(3)),
        None
    );
    // 投递重试（任务表幂等分支再次回填同一结果）：不得重复累计。
    assert_eq!(
        runner.note_task_execution("run-201", "t-1", 0, &tool_only_result(3)),
        None,
        "同一 task 的重复回填只计首次"
    );
    // 另一任务继续累计：3+3 > 5 说明首次的 3 未被去重吞掉。
    assert_eq!(
        runner
            .note_task_execution("run-201", "t-2", 0, &tool_only_result(3))
            .as_deref(),
        Some("workflow exceeded maxToolCalls (5)")
    );
}

/// 真实引擎集成用例的显式前置：由开发者提供已校验引擎包目录
/// （`PERI_WORKFLOW_ARTIFACT_DIR`）。仓库不读工作树 dist、也不假设本机装有引擎：
/// 未提供时打印说明并返回（显式、可见，不用 `#[ignore]`）。
fn require_explicit_engine() -> bool {
    if std::env::var_os("PERI_WORKFLOW_ARTIFACT_DIR").is_some_and(|value| !value.is_empty()) {
        return true;
    }
    eprintln!(
        "note: 跳过真实引擎用例（仓库不读工作树 dist）；         需要时设置 PERI_WORKFLOW_ARTIFACT_DIR=<@peri-code/workflow 包目录> 后重跑"
    );
    false
}

fn completed_workflow_result() -> WorkflowResult {
    WorkflowResult {
        run_id: "run-fast".into(),
        status: "completed".into(),
        return_value: None,
        error: None,
        post_processing_status: peri_acp_types::workflow::PostProcessingStatus::NotRequired,
        delivery_status: peri_acp_types::workflow::DeliveryStatus::Blocked,
        stderr_tail: None,
        unknown_side_effects: false,
    }
}

#[tokio::test]
async fn workflow_result_receiver_reads_already_published_value() {
    let (done_tx, done_rx) = tokio::sync::watch::channel(None);
    done_tx
        .send(Some(completed_workflow_result()))
        .expect("done tx send test receiver must remain available for this delivery");
    let mut late_rx = done_rx.clone();
    drop(done_tx);

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        crate::runner::receive_workflow_result(&mut late_rx),
    )
    .await
    .expect("已发布的 watch 终态必须立即可读")
    .expect("已发布的终态不得丢失");

    assert_eq!(result.status, "completed");
}

#[tokio::test]
async fn workflow_result_receiver_returns_none_when_closed_without_result() {
    let (done_tx, mut done_rx) = tokio::sync::watch::channel::<Option<WorkflowResult>>(None);
    drop(done_tx);

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        crate::runner::receive_workflow_result(&mut done_rx),
    )
    .await
    .expect("关闭且无终态的 watch 不得挂起");

    assert!(result.is_none());
}

fn captured_git_observation() -> crate::journal::GitObservation {
    crate::journal::GitObservation::Captured(crate::journal::GitBaseline {
        repo_root: std::path::PathBuf::from("/repo"),
        cwd: std::path::PathBuf::from("/repo"),
        git_dir: std::path::PathBuf::from("/repo/.git"),
        git_common_dir: std::path::PathBuf::from("/repo/.git"),
        head: "0123456789abcdef".into(),
        status_porcelain_v2: Vec::new(),
    })
}

/// 内核只投影 `blocked` / `unknown`：acceptance 无生产写入方，任何组合都不会
/// 升级成 `deliverable`（该判定属引擎之外的验收层）。
#[test]
fn postcondition_projection_never_escalates_to_deliverable() {
    use peri_acp_types::workflow::{
        AcceptanceStatus, DeliveryStatus, PostProcessingStatus, WorkflowWriteIntent,
    };

    let read_only = WorkflowWriteIntent::ReadOnly;
    let captured = captured_git_observation();
    let project = |acceptance| {
        crate::runner::terminal::project_postcondition(
            "completed",
            acceptance,
            Some(&read_only),
            &captured,
            Some(&Ok(())),
        )
    };
    for acceptance in [AcceptanceStatus::Unknown, AcceptanceStatus::Passed] {
        let projection = project(acceptance);
        assert_eq!(
            (projection.post_processing, projection.delivery),
            (PostProcessingStatus::NotRequired, DeliveryStatus::Unknown)
        );
        assert_eq!(projection.reason, None);
    }
    // acceptance 无生产写入方（内核不判验收）：Passed 也不把 delivery 抬到
    // deliverable——deliverable 属引擎之外的验收层（2026-10-11 用户裁决）。
    let projection = project(AcceptanceStatus::Failed);
    assert_eq!(
        (projection.post_processing, projection.delivery),
        (PostProcessingStatus::NotRequired, DeliveryStatus::Blocked)
    );
}

#[test]
fn postcondition_projection_fails_safe_without_evidence() {
    use peri_acp_types::workflow::{
        AcceptanceStatus, DeliveryReason, DeliveryStatus, PostProcessingStatus,
        PostconditionObservation, WorkflowWriteIntent,
    };

    let write = WorkflowWriteIntent::Write {
        repo_root: "/repo".into(),
        cwd: "/repo".into(),
        path_allowlist: vec!["src".into()],
        head_may_change: false,
        commit_required: None,
    };
    let captured = captured_git_observation();
    // 有基线但没跑验证：不能假定通过。
    let projection = crate::runner::terminal::project_postcondition(
        "completed",
        AcceptanceStatus::Unknown,
        Some(&write),
        &captured,
        None,
    );
    assert_eq!(
        (projection.post_processing, projection.delivery),
        (PostProcessingStatus::Blocked, DeliveryStatus::Blocked)
    );
    assert_eq!(
        projection.reason,
        Some(DeliveryReason::ObservationIncomplete)
    );
    assert_eq!(projection.observation, PostconditionObservation::Incomplete);

    let projection = crate::runner::terminal::project_postcondition(
        "completed",
        AcceptanceStatus::Unknown,
        Some(&write),
        &captured,
        Some(&Err(crate::journal::PostconditionFailure::Violation(
            "credential token=secret".into(),
        ))),
    );
    assert_eq!(
        (projection.post_processing, projection.delivery),
        (PostProcessingStatus::Failed, DeliveryStatus::Blocked)
    );
    assert_eq!(projection.reason, None);

    // 采样失败（含非 Git 上的显式只读）：blocked + observation_incomplete，
    // 绝不显示通过。
    for observation in [
        crate::journal::GitObservation::Incomplete("git failed".into()),
        crate::journal::GitObservation::NotApplicable,
    ] {
        let projection = crate::runner::terminal::project_postcondition(
            "completed",
            AcceptanceStatus::Unknown,
            Some(&WorkflowWriteIntent::ReadOnly),
            &observation,
            Some(&Ok(())),
        );
        assert_eq!(
            (projection.post_processing, projection.delivery),
            (PostProcessingStatus::Blocked, DeliveryStatus::Blocked)
        );
        assert_eq!(
            projection.reason,
            Some(DeliveryReason::ObservationIncomplete)
        );
        assert_eq!(projection.observation, PostconditionObservation::Incomplete);
    }
}

/// late settle 的致命失败没有结构化 breach：它必须在投影前把 completed 归一化为
/// failed，否则 delivery 会停在 `unknown` / `not_requested`。
#[test]
fn late_fatal_failure_is_normalized_before_projection() {
    use crate::runner::terminal::{project_postcondition, terminal_status};
    use peri_acp_types::workflow::{AcceptanceStatus, DeliveryStatus};

    let status = terminal_status("completed", &[], Some("scope not closed"));
    assert_eq!(status, "failed");
    let projection = project_postcondition(
        status,
        AcceptanceStatus::Unknown,
        None,
        &captured_git_observation(),
        Some(&Ok(())),
    );
    assert_eq!(projection.delivery, DeliveryStatus::Blocked);
    assert_eq!(projection.reason, None);

    assert_eq!(terminal_status("completed", &[], None), "completed");
    assert_eq!(
        terminal_status("killed", &[], Some("scope not closed")),
        "killed"
    );
}

#[test]
fn postcondition_projection_separates_unattributed_changes_from_violations() {
    use peri_acp_types::workflow::{AcceptanceStatus, DeliveryStatus, PostProcessingStatus};

    let read_only = peri_acp_types::workflow::WorkflowWriteIntent::ReadOnly;
    let captured = captured_git_observation();
    let unattributed = crate::journal::PostconditionFailure::Unattributed(
        "git changes outside the run's write footprint".into(),
    );
    let projection = crate::runner::terminal::project_postcondition(
        "completed",
        AcceptanceStatus::Unknown,
        Some(&read_only),
        &captured,
        Some(&Err(unattributed)),
    );
    assert_eq!(
        (projection.post_processing, projection.delivery),
        (PostProcessingStatus::Blocked, DeliveryStatus::Blocked)
    );
    assert_eq!(
        projection.reason,
        Some(peri_acp_types::workflow::DeliveryReason::UnattributedChange)
    );

    let violation = crate::journal::PostconditionFailure::Violation("escaped allowlist".into());
    let projection = crate::runner::terminal::project_postcondition(
        "completed",
        AcceptanceStatus::Unknown,
        Some(&read_only),
        &captured,
        Some(&Err(violation)),
    );
    assert_eq!(
        (projection.post_processing, projection.delivery),
        (PostProcessingStatus::Failed, DeliveryStatus::Blocked)
    );
}

#[test]
fn terminal_git_error_assembly_includes_each_attribution_tier() {
    use crate::journal::PostconditionFailure;

    assert_eq!(
        crate::runner::terminal::postcondition_error(&PostconditionFailure::Violation(
            "escaped writeIntent.path_allowlist".into()
        )),
        "Git postcondition failed: run-owned violation: escaped writeIntent.path_allowlist"
    );
    assert_eq!(
        crate::runner::terminal::postcondition_error(&PostconditionFailure::Unattributed(
            "git HEAD changed outside the run's write footprint".into()
        )),
        "Git postcondition blocked: unattributed change (external or unknown): git HEAD changed outside the run's write footprint"
    );
    assert_eq!(
        crate::runner::terminal::postcondition_error(&PostconditionFailure::ObservationIncomplete(
            "cannot sample".into()
        )),
        "Git postcondition blocked: observation incomplete: cannot sample"
    );
}

/// 投影表：每一行的证据组合都必须落到对应结论，且旧枚举不新增值。
#[test]
fn postcondition_projection_table_grades_delivery_by_evidence() {
    use peri_acp_types::workflow::{
        AcceptanceStatus, DeliveryReason, DeliveryStatus, PostProcessingStatus,
        PostconditionObservation, WorkflowWriteIntent,
    };

    let write = WorkflowWriteIntent::Write {
        repo_root: "/repo".into(),
        cwd: "/repo".into(),
        path_allowlist: vec!["src".into()],
        head_may_change: false,
        commit_required: None,
    };
    let captured = captured_git_observation();

    // 无 intent + 观测完整 + 无差异：交付未请求，不阻塞，旧枚举保持 unknown。
    let projection = crate::runner::terminal::project_postcondition(
        "completed",
        AcceptanceStatus::Unknown,
        None,
        &captured,
        Some(&Ok(())),
    );
    assert_eq!(
        (projection.post_processing, projection.delivery),
        (PostProcessingStatus::Unknown, DeliveryStatus::Unknown)
    );
    assert_eq!(projection.reason, Some(DeliveryReason::NotRequested));
    assert_eq!(projection.observation, PostconditionObservation::Complete);

    // 非 Git + 无 intent：not_requested，观测原因 not_applicable。
    let projection = crate::runner::terminal::project_postcondition(
        "completed",
        AcceptanceStatus::Unknown,
        None,
        &crate::journal::GitObservation::NotApplicable,
        None,
    );
    assert_eq!(
        (projection.post_processing, projection.delivery),
        (PostProcessingStatus::Unknown, DeliveryStatus::Unknown)
    );
    assert_eq!(projection.reason, Some(DeliveryReason::NotRequested));
    assert_eq!(
        projection.observation,
        PostconditionObservation::NotApplicable
    );

    // 无 intent 但观察到变化：仍 blocked，不升级 failed。
    let projection = crate::runner::terminal::project_postcondition(
        "completed",
        AcceptanceStatus::Unknown,
        None,
        &captured,
        Some(&Err(crate::journal::PostconditionFailure::Unattributed(
            "changed".into(),
        ))),
    );
    assert_eq!(
        (projection.post_processing, projection.delivery),
        (PostProcessingStatus::Blocked, DeliveryStatus::Blocked)
    );
    assert_eq!(projection.reason, Some(DeliveryReason::UnattributedChange));

    // 显式写 intent 通过：既有投影不退化。
    let projection = crate::runner::terminal::project_postcondition(
        "completed",
        AcceptanceStatus::Unknown,
        Some(&write),
        &captured,
        Some(&Ok(())),
    );
    assert_eq!(
        (projection.post_processing, projection.delivery),
        (PostProcessingStatus::Passed, DeliveryStatus::Unknown)
    );
    assert_eq!(projection.reason, None);

    // 失败/取消的运行不因“交付不需要”而变绿；原因由 execution 表达。
    for execution in ["failed", "killed"] {
        let projection = crate::runner::terminal::project_postcondition(
            execution,
            AcceptanceStatus::Unknown,
            None,
            &captured,
            Some(&Ok(())),
        );
        assert_eq!(projection.delivery, DeliveryStatus::Blocked);
        assert_eq!(projection.reason, None);
    }
}

#[test]
fn outcome_separates_execution_from_delivery() {
    use peri_acp_types::workflow::{
        AcceptanceStatus, DeliveryStatus, ExecutionStatus, WorkflowOutcome,
    };

    assert_eq!(
        WorkflowOutcome::project(
            ExecutionStatus::Completed,
            AcceptanceStatus::Unknown,
            DeliveryStatus::Unknown,
        ),
        WorkflowOutcome::Completed
    );
    assert_eq!(
        WorkflowOutcome::project(
            ExecutionStatus::Completed,
            AcceptanceStatus::Unknown,
            DeliveryStatus::Blocked,
        ),
        WorkflowOutcome::Blocked
    );
    assert_eq!(
        WorkflowOutcome::project(
            ExecutionStatus::Completed,
            AcceptanceStatus::Failed,
            DeliveryStatus::Unknown,
        ),
        WorkflowOutcome::Blocked
    );
    assert_eq!(
        WorkflowOutcome::project(
            ExecutionStatus::Failed,
            AcceptanceStatus::Unknown,
            DeliveryStatus::Blocked,
        ),
        WorkflowOutcome::Failed
    );
    assert_eq!(
        WorkflowOutcome::project(
            ExecutionStatus::Running,
            AcceptanceStatus::Unknown,
            DeliveryStatus::Unknown,
        ),
        WorkflowOutcome::Unknown
    );
}

#[test]
fn failed_terminal_without_error_gets_a_readable_fallback() {
    let mut result = completed_workflow_result();
    result.status = "failed".into();
    result.error = None;
    crate::runner::terminal::ensure_failure_error(&mut result);
    assert_eq!(
        result.error.as_deref(),
        Some("workflow failed without error details")
    );
    result.error = Some("   ".into());
    crate::runner::terminal::ensure_failure_error(&mut result);
    assert_eq!(
        result.error.as_deref(),
        Some("workflow failed without error details")
    );
}

#[test]
fn test_workflow_start_params_preserve_budget_total() {
    let input = WorkflowInput {
        script: "export const meta = { name: 'budget', description: 'test' }".to_string(),
        args: Some(serde_json::json!({"key": "value"})),
        max_concurrency: 2,
        budget_total: Some(9_007_199_254_740_991),
        limits: WorkflowLimits::default(),
        workflow_name: "budget".to_string(),
        resume_from: None,
        write_intent: None,
        git_observation: Default::default(),
    };

    let params =
        crate::runner::run_protocol::workflow_start_params("run-1", &input, None, None, "/tmp");

    assert_eq!(params.budget_total, Some(9_007_199_254_740_991));
    assert_eq!(params.max_concurrency, 2);
    assert_eq!(params.args, input.args);
    assert!(params.resume_tools.is_none());
}

#[test]
fn test_agent_task_params_preserve_requested_model() {
    let (task_id, params) = crate::runner::run_protocol::parse_agent_task_params(
        Some(serde_json::json!({
            "runId": "run-1",
            "taskId": "t-1",
            "agentId": 7,
            "prompt": "inspect",
            "model": "sonnet"
        })),
        "run-1",
    )
    .expect("the parse agent task params fixture result is required for this assertion");

    assert_eq!(task_id, "t-1");
    assert_eq!(params.model.as_deref(), Some("sonnet"));
}

#[test]
fn test_agent_task_params_reject_invalid_model_type() {
    let result = crate::runner::run_protocol::parse_agent_task_params(
        Some(serde_json::json!({
            "runId": "run-1",
            "taskId": "t-1",
            "agentId": 7,
            "prompt": "inspect",
            "model": 42
        })),
        "run-1",
    );

    assert!(result.is_err());
}

#[test]
fn test_agent_task_params_reject_missing_params() {
    assert!(crate::runner::run_protocol::parse_agent_task_params(None, "run-1").is_err());
}

#[test]
fn test_agent_task_params_reject_empty_task_id() {
    let result = crate::runner::run_protocol::parse_agent_task_params(
        Some(serde_json::json!({
            "runId": "run-1",
            "taskId": "  ",
            "agentId": 7,
            "prompt": "inspect"
        })),
        "run-1",
    );

    assert_eq!(result.unwrap_err(), "taskId must be a non-empty string");
}

include!("runner_test_section_2.rs");
