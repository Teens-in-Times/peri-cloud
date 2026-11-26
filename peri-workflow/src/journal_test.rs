use crate::journal::git::GitBaseline;
use crate::journal::git::GitObservation;
use crate::journal::AttemptTracker;
use crate::journal::RunState;
use crate::journal::WorkflowJournalStore;
use crate::protocol::JournalEntry;
use crate::protocol::{AgentRunResult, Usage};
use peri_acp_types::workflow::AcceptanceStatus;
use peri_acp_types::workflow::AttemptDisposition;
use peri_acp_types::workflow::DeliveryStatus;
use peri_acp_types::workflow::ExecutionStatus;
use peri_acp_types::workflow::PostProcessingStatus;
use peri_acp_types::workflow::WorkflowAttempt;

use tempfile::TempDir;

fn make_store() -> (TempDir, WorkflowJournalStore) {
    let tmp = TempDir::new().expect("the new fixture result is required for this assertion");
    let store = WorkflowJournalStore::new(
        tmp.path()
            .to_str()
            .expect("the fixture text must be valid UTF-8"),
    );
    (tmp, store)
}

#[test]
fn test_init_run_creates_dir_and_script() {
    let (_tmp, store) = make_store();
    store
        .init_run("run-1", "export const meta = {}")
        .expect("the init run fixture result is required for this assertion");
    let script = std::fs::read_to_string(store.run_dir("run-1").join("script.js"))
        .expect("the test fixture text file must be readable");
    assert_eq!(script, "export const meta = {}");
}

#[test]
fn test_append_and_read_all_journal() {
    let (_tmp, store) = make_store();
    store
        .init_run("run-1", "script")
        .expect("the init run fixture result is required for this assertion");
    let entry = JournalEntry {
        key: "abc123".into(),
        seq: 0,
        result: AgentRunResult::Ok {
            output: "hello".into(),
            usage: Usage { output_tokens: 10 },
            model: None,
            tool_count: None,
            token_count: None,
            phase: None,
            duration_ms: None,
        },
        attempt: None,
    };
    store
        .append("run-1", &entry)
        .expect("the append fixture result is required for this assertion");
    let entries = store
        .read_all("run-1")
        .expect("the read all fixture result is required for this assertion");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].key, "abc123");
    assert_eq!(entries[0].seq, 0);
}

#[test]
fn strict_resume_read_rejects_missing_and_malformed_journal() {
    let (_tmp, store) = make_store();
    store
        .init_run("empty", "script")
        .expect("the init run fixture result is required for this assertion");
    assert!(
        store
            .read_all_strict("empty")
            .expect("init_run 建出的空 journal 必须可读")
            .is_empty(),
        "零 agent run 的空 journal 是合法空前缀"
    );

    std::fs::create_dir_all(store.run_dir("missing"))
        .expect("the missing-run fixture directory must be creatable");
    assert!(store.read_all_strict("missing").is_err());

    store
        .init_run("malformed", "script")
        .expect("the init run fixture result is required for this assertion");
    std::fs::write(
        store.run_dir("malformed").join("journal.jsonl"),
        "{\"key\":\"ok\",\"seq\":0,\"result\":{\"kind\":\"ok\",\"output\":\"ok\",\"usage\":{\"outputTokens\":1}}}\nnot-json\n",
    )
    .expect("the test fixture file must be writable");
    let error = store.read_all_strict("malformed").unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("line 2"));

    store
        .init_run("gap", "script")
        .expect("the init run fixture result is required for this assertion");
    std::fs::write(
        store.run_dir("gap").join("journal.jsonl"),
        "{\"key\":\"later\",\"seq\":1,\"result\":{\"kind\":\"ok\",\"output\":\"later\",\"usage\":{\"outputTokens\":1}}}\n",
    )
    .expect("the test fixture file must be writable");
    let error = store.read_all_strict("gap").unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("expected seq 0"));

    store
        .init_run("duplicate", "script")
        .expect("the init run fixture result is required for this assertion");
    std::fs::write(
        store.run_dir("duplicate").join("journal.jsonl"),
        "{\"key\":\"a\",\"seq\":0,\"result\":{\"kind\":\"ok\",\"output\":\"a\",\"usage\":{\"outputTokens\":1}}}\n{\"key\":\"b\",\"seq\":0,\"result\":{\"kind\":\"ok\",\"output\":\"b\",\"usage\":{\"outputTokens\":1}}}\n",
    )
    .expect("the test fixture file must be writable");
    let error = store.read_all_strict("duplicate").unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("expected seq 1"));
}

// ─── tools.jsonl（issue #407 阶段 2）：append-only 工具调用记录 ───────────────

fn tool_entry(seq: u64, agent_seq: u64, task: &str) -> crate::protocol::ToolJournalEntry {
    crate::protocol::ToolJournalEntry {
        tool_seq: seq,
        agent_seq,
        task_id: task.into(),
        tool_name: "Read".into(),
        arguments: serde_json::Map::new(),
        outcome: peri_acp_types::workflow::WorkflowToolOutcome::Ok {
            structured_content: None,
            content: format!("result-{seq}"),
        },
    }
}

#[test]
fn tool_journal_appends_and_reads_back_in_sorted_order() {
    let (_tmp, store) = make_store();
    store
        .init_run("run-1", "script")
        .expect("the init run fixture result is required for this assertion");
    store
        .append_tool("run-1", &tool_entry(0, 1, "tool-1"))
        .expect("append_tool must write the fixture");
    store
        .append_tool("run-1", &tool_entry(1, 2, "tool-2"))
        .expect("append_tool must write the fixture");

    let entries = store
        .read_tools_strict("run-1")
        .expect("read_tools_strict must accept the written fixture");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].task_id, "tool-1");
    assert_eq!(entries[0].tool_seq, 0);
    assert_eq!(entries[0].agent_seq, 1);
    assert_eq!(entries[1].task_id, "tool-2");
    assert_eq!(entries[1].tool_seq, 1);
    assert_eq!(entries[1].agent_seq, 2);
}

#[test]
fn missing_tool_journal_reads_as_empty() {
    let (_tmp, store) = make_store();
    store
        .init_run("run-1", "script")
        .expect("the init run fixture result is required for this assertion");
    assert!(store
        .read_tools_strict("run-1")
        .expect("缺失 tools.jsonl 不是错误")
        .is_empty());
}

#[test]
fn tool_journal_reads_out_of_order_writes_by_tool_seq() {
    let (_tmp, store) = make_store();
    store
        .init_run("run-1", "script")
        .expect("the init run fixture result is required for this assertion");
    // 并发工具调用按完成顺序追加：文件顺序可以不是脚本调用顺序。
    store
        .append_tool("run-1", &tool_entry(1, 0, "tool-2"))
        .expect("append_tool must write the fixture");
    store
        .append_tool("run-1", &tool_entry(0, 0, "tool-1"))
        .expect("append_tool must write the fixture");

    let entries = store
        .read_tools_strict("run-1")
        .expect("read_tools_strict must accept the written fixture");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].tool_seq, 0);
    assert_eq!(entries[0].task_id, "tool-1");
    assert_eq!(entries[1].tool_seq, 1);
    assert_eq!(entries[1].task_id, "tool-2");
}

#[test]
fn malformed_tool_journal_fails_closed() {
    let (_tmp, store) = make_store();
    store
        .init_run("run-1", "script")
        .expect("the init run fixture result is required for this assertion");
    std::fs::write(
        store.run_dir("run-1").join("tools.jsonl"),
        "{\"toolSeq\":0,\"agentSeq\":0,\"taskId\":\"tool-1\",\"toolName\":\"Read\",\"arguments\":{},\"outcome\":{\"kind\":\"ok\",\"content\":\"ok\"}}\nnot-json\n",
    )
    .expect("the test fixture file must be writable");

    let error = store.read_tools_strict("run-1").unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("line 2"));
}

#[test]
fn truncate_does_not_clear_the_tool_journal() {
    let (_tmp, store) = make_store();
    store
        .init_run("run-1", "script")
        .expect("the init run fixture result is required for this assertion");
    store
        .append_tool("run-1", &tool_entry(0, 0, "tool-1"))
        .expect("append_tool must write the fixture");
    store
        .truncate("run-1")
        .expect("truncate must clear only journal.jsonl");
    assert_eq!(
        store
            .read_tools_strict("run-1")
            .expect("read_tools_strict must accept the fixture")
            .len(),
        1,
        "tools.jsonl 是 append-only：journal/truncate 不得清空"
    );
}

#[test]
fn test_truncate_clears_journal() {
    let (_tmp, store) = make_store();
    store
        .init_run("run-1", "script")
        .expect("the init run fixture result is required for this assertion");
    let entry = JournalEntry {
        key: "k".into(),
        seq: 0,
        result: AgentRunResult::Skipped,
        attempt: None,
    };
    store
        .append("run-1", &entry)
        .expect("the append fixture result is required for this assertion");
    store
        .truncate("run-1")
        .expect("the truncate fixture result is required for this assertion");
    let entries = store
        .read_all("run-1")
        .expect("the read all fixture result is required for this assertion");
    assert!(entries.is_empty());
}

#[test]
fn test_write_and_read_state() {
    let (_tmp, store) = make_store();
    store
        .init_run("run-1", "script")
        .expect("the init run fixture result is required for this assertion");
    let state = RunState {
        run_id: "run-1".into(),
        workflow_name: "test".into(),
        status: "completed".into(),
        execution_status: ExecutionStatus::Completed,
        acceptance_status: AcceptanceStatus::Unknown,
        post_processing_status: PostProcessingStatus::Blocked,
        delivery_status: DeliveryStatus::Blocked,
        delivery_reason: None,
        postcondition_observation: None,
        git_observation_scope: None,
        outcome: peri_acp_types::workflow::WorkflowOutcome::Blocked,
        unknown_side_effects: false,
        write_intent: None,
        limits: crate::protocol::WorkflowLimits::default(),
        limit_breaches: Vec::new(),
        token_usage: None,
        budget_total: Some(9_007_199_254_740_991),
        args: Some(serde_json::json!({"task": "round-trip"})),
        max_concurrency: 7,
        attempts: Vec::new(),
        return_value: Some(serde_json::json!({"ok": true})),
        script: "script".into(),
        started_at: "2026-06-22T00:00:00Z".into(),
        finished_at: Some("2026-06-22T00:01:00Z".into()),
        error: None,
    };
    store
        .write_state("run-1", &state)
        .expect("the write state fixture result is required for this assertion");
    let read = store
        .read_state("run-1")
        .expect("the read state fixture result is required for this assertion");
    assert_eq!(read.run_id, "run-1");
    assert_eq!(read.status, "completed");
    assert_eq!(read.budget_total, Some(9_007_199_254_740_991));
    assert_eq!(read.args, Some(serde_json::json!({"task": "round-trip"})));
    assert_eq!(read.max_concurrency, 7);
    assert_eq!(
        read.outcome,
        peri_acp_types::workflow::WorkflowOutcome::Blocked,
        "state.json 必须保留独立的引擎级 outcome"
    );
}

#[test]
fn test_legacy_state_defaults_budget_total_to_none() {
    let state: RunState = serde_json::from_value(serde_json::json!({
        "run_id": "legacy-budget",
        "workflow_name": "legacy",
        "status": "completed",
        "script": "script",
        "started_at": "2026-06-22T00:00:00Z"
    }))
    .expect("from_value must accept this test fixture representation");

    assert_eq!(state.budget_total, None);
    assert_eq!(state.args, None);
    assert_eq!(state.max_concurrency, 3);
}

#[test]
fn test_state_error_field_round_trip() {
    // failed 的 state.json 必须能保存并读回 error 字段
    let (_tmp, store) = make_store();
    store
        .init_run("run-err", "script")
        .expect("the init run fixture result is required for this assertion");
    let state = RunState {
        run_id: "run-err".into(),
        workflow_name: "test".into(),
        status: "failed".into(),
        execution_status: ExecutionStatus::Failed,
        acceptance_status: AcceptanceStatus::Unknown,
        post_processing_status: PostProcessingStatus::Failed,
        delivery_status: DeliveryStatus::Blocked,
        delivery_reason: None,
        postcondition_observation: None,
        git_observation_scope: None,
        outcome: peri_acp_types::workflow::WorkflowOutcome::Blocked,
        unknown_side_effects: false,
        write_intent: None,
        limits: crate::protocol::WorkflowLimits::default(),
        limit_breaches: Vec::new(),
        token_usage: None,
        budget_total: None,
        args: None,
        max_concurrency: 3,
        attempts: Vec::new(),
        return_value: None,
        script: "script".into(),
        started_at: "2026-06-22T00:00:00Z".into(),
        finished_at: Some("2026-06-22T00:00:01Z".into()),
        error: Some("parallel thunk #0 failed: t is not a function".into()),
    };
    store
        .write_state("run-err", &state)
        .expect("the write state fixture result is required for this assertion");
    let read = store
        .read_state("run-err")
        .expect("the read state fixture result is required for this assertion");
    assert_eq!(read.status, "failed");
    assert_eq!(
        read.error.as_deref(),
        Some("parallel thunk #0 failed: t is not a function")
    );
}

#[test]
fn test_state_error_skipped_when_none() {
    // error 为 None 时序列化应省略该字段（向后兼容旧 state.json）
    let (_tmp, store) = make_store();
    store
        .init_run("run-ok", "script")
        .expect("the init run fixture result is required for this assertion");
    let state = RunState {
        run_id: "run-ok".into(),
        workflow_name: "test".into(),
        status: "completed".into(),
        execution_status: ExecutionStatus::Completed,
        acceptance_status: AcceptanceStatus::Passed,
        post_processing_status: PostProcessingStatus::NotRequired,
        delivery_status: DeliveryStatus::Unknown,
        delivery_reason: None,
        postcondition_observation: None,
        git_observation_scope: None,
        outcome: peri_acp_types::workflow::WorkflowOutcome::Completed,
        unknown_side_effects: false,
        write_intent: None,
        limits: crate::protocol::WorkflowLimits::default(),
        limit_breaches: Vec::new(),
        token_usage: None,
        budget_total: None,
        args: None,
        max_concurrency: 3,
        attempts: Vec::new(),
        return_value: None,
        script: "script".into(),
        started_at: "2026-06-22T00:00:00Z".into(),
        finished_at: None,
        error: None,
    };
    store
        .write_state("run-ok", &state)
        .expect("the write state fixture result is required for this assertion");
    let raw = std::fs::read_to_string(store.run_dir("run-ok").join("state.json"))
        .expect("the test fixture text file must be readable");
    assert!(!raw.contains("error"), "None 时不应序列化 error 字段");
}

#[test]
fn test_legacy_deliverable_state_reads_as_unknown() {
    let state: RunState = serde_json::from_value(serde_json::json!({
        "run_id": "legacy-delivery",
        "workflow_name": "legacy",
        "status": "completed",
        "delivery_status": "deliverable",
        "script": "script",
        "started_at": "2026-06-22T00:00:00Z"
    }))
    .expect("from_value must accept this test fixture representation");

    assert_eq!(
        state.delivery_status,
        DeliveryStatus::Unknown,
        "历史 state.json 的 deliverable 必须按 unknown 读入（数据兼容），不得反序列化失败"
    );
}

#[test]
fn test_legacy_completed_defaults_do_not_claim_delivery() {
    let state: RunState = serde_json::from_value(serde_json::json!({
        "run_id": "legacy-run",
        "workflow_name": "legacy",
        "status": "completed",
        "script": "script",
        "started_at": "2026-06-22T00:00:00Z",
        "future_field": {"ignored": true}
    }))
    .expect("from_value must accept this test fixture representation");

    assert_eq!(state.status, "completed");
    assert_eq!(state.execution_status, ExecutionStatus::Unknown);
    assert_eq!(state.acceptance_status, AcceptanceStatus::Unknown);
    assert_eq!(state.post_processing_status, PostProcessingStatus::Unknown);
    assert_eq!(state.delivery_status, DeliveryStatus::Unknown);
    assert_eq!(
        state.delivery_reason, None,
        "旧 state.json 缺原因字段：不得据此推断“未请求交付”或“无变化”"
    );
    assert_eq!(state.postcondition_observation, None);
}

#[test]
fn postcondition_reason_fields_round_trip_and_stay_optional() {
    let tmp = TempDir::new().expect("the temporary test directory must be available");
    let store = WorkflowJournalStore::new(
        tmp.path()
            .to_str()
            .expect("the fixture text must be valid UTF-8"),
    );
    store
        .init_run("run-reason", "script")
        .expect("the init run fixture result is required for this assertion");
    let state: RunState = serde_json::from_value(serde_json::json!({
        "run_id": "run-reason",
        "workflow_name": "reason",
        "status": "completed",
        "script": "script",
        "started_at": "2026-06-22T00:00:00Z"
    }))
    .expect("legacy payload must deserialize");
    let mut state = state;
    state.delivery_reason = Some(peri_acp_types::workflow::DeliveryReason::NotRequested);
    state.postcondition_observation =
        Some(peri_acp_types::workflow::PostconditionObservation::Complete);
    store
        .write_state("run-reason", &state)
        .expect("the write state fixture result is required for this assertion");

    let read = store
        .read_state("run-reason")
        .expect("the read state fixture result is required for this assertion");
    assert_eq!(
        read.delivery_reason,
        Some(peri_acp_types::workflow::DeliveryReason::NotRequested)
    );
    assert_eq!(
        read.postcondition_observation,
        Some(peri_acp_types::workflow::PostconditionObservation::Complete)
    );
}

#[test]
fn test_non_git_directory_reports_not_applicable_without_intent() {
    let tmp = TempDir::new().expect("the new fixture result is required for this assertion");

    assert!(matches!(
        GitBaseline::capture_for_intent(tmp.path(), None)
            .expect("sampling failure must not fail start"),
        GitObservation::NotApplicable
    ));
    assert!(matches!(
        GitBaseline::capture_for_intent(
            tmp.path(),
            Some(&peri_acp_types::workflow::WorkflowWriteIntent::ReadOnly),
        )
        .expect("read-only sampling failure must not fail start"),
        GitObservation::NotApplicable
    ));
}

#[test]
fn test_non_git_baseline_is_required_for_write_intent() {
    let tmp = TempDir::new().expect("the new fixture result is required for this assertion");
    let intent = peri_acp_types::workflow::WorkflowWriteIntent::Write {
        repo_root: tmp.path().to_string_lossy().into_owned(),
        cwd: tmp.path().to_string_lossy().into_owned(),
        path_allowlist: vec!["allowed.txt".into()],
        head_may_change: false,
        commit_required: None,
    };

    let error = GitBaseline::capture_for_intent(tmp.path(), Some(&intent)).unwrap_err();
    // 采样失败必须保留 git 自身 stderr 摘要，不能退化成统一文本。
    let detail = error
        .strip_prefix("git rev-parse --show-toplevel failed: ")
        .unwrap_or_else(|| panic!("错误必须带失败命令: {error}"));
    assert!(
        !detail.trim().is_empty() && detail != "no stderr output",
        "错误必须保留 git 诊断: {error}"
    );
    assert!(
        detail.chars().count() <= 501,
        "stderr 摘要必须有界: {error}"
    );
}

#[test]
fn test_git_baseline_preserves_dirty_staged_and_untracked_bytes() {
    let tmp = TempDir::new().expect("the new fixture result is required for this assertion");
    let run_git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(tmp.path())
            .status()
            .expect("the status fixture result is required for this assertion");
        assert!(status.success());
    };
    run_git(&["init", "-q"]);
    std::fs::write(tmp.path().join("tracked.txt"), b"base\n")
        .expect("the test fixture file must be writable");
    run_git(&["add", "tracked.txt"]);
    run_git(&[
        "-c",
        "user.name=Peri Test",
        "-c",
        "user.email=peri-test@example.invalid",
        "commit",
        "-qm",
        "baseline",
    ]);

    std::fs::write(tmp.path().join("tracked.txt"), b"staged\n")
        .expect("the test fixture file must be writable");
    run_git(&["add", "tracked.txt"]);
    std::fs::write(tmp.path().join("tracked.txt"), b"dirty\n")
        .expect("the test fixture file must be writable");
    std::fs::write(tmp.path().join("untracked.bin"), [0_u8, 1, 2, 255])
        .expect("the test fixture file must be writable");
    let tracked_before = std::fs::read(tmp.path().join("tracked.txt"))
        .expect("the test fixture file must be readable");
    let untracked_before = std::fs::read(tmp.path().join("untracked.bin"))
        .expect("the test fixture file must be readable");

    let baseline = GitBaseline::capture(tmp.path())
        .expect("the capture fixture result is required for this assertion");
    baseline
        .verify_unchanged()
        .expect("the verify unchanged fixture result is required for this assertion");

    assert_eq!(
        std::fs::read(tmp.path().join("tracked.txt"))
            .expect("the test fixture file must be readable"),
        tracked_before
    );
    assert_eq!(
        std::fs::read(tmp.path().join("untracked.bin"))
            .expect("the test fixture file must be readable"),
        untracked_before
    );
}

#[test]
fn test_git_baseline_detects_unattributed_change_without_restoring_it() {
    let tmp = TempDir::new().expect("the new fixture result is required for this assertion");
    let run_git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(tmp.path())
            .status()
            .expect("the status fixture result is required for this assertion");
        assert!(status.success());
    };
    run_git(&["init", "-q"]);
    std::fs::write(tmp.path().join("tracked.txt"), "base\n")
        .expect("the test fixture file must be writable");
    run_git(&["add", "tracked.txt"]);
    run_git(&[
        "-c",
        "user.name=Peri Test",
        "-c",
        "user.email=peri-test@example.invalid",
        "commit",
        "-qm",
        "baseline",
    ]);

    let baseline = GitBaseline::capture(tmp.path())
        .expect("the capture fixture result is required for this assertion");
    std::fs::write(
        tmp.path().join("tracked.txt"),
        "agent claim without ownership\n",
    )
    .expect("the test fixture file must be writable");

    let error = baseline.verify_unchanged().unwrap_err();
    assert!(
        !error.is_violation(),
        "无 write footprint 时变化只能判为未归因"
    );
    let message = error.to_string();
    assert!(message.contains("outside the run's write footprint"));
    assert!(
        message.contains("tracked.txt"),
        "报告必须带可定位路径: {message}"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("tracked.txt"))
            .expect("the test fixture text file must be readable"),
        "agent claim without ownership\n"
    );
}

/// 未合并条目（`u`）也要能被解析：始终比较 map（而非原始字节）后，
/// 冲突中的仓库不能因为解析失败被读成观测不完整。
#[test]
fn unmerged_porcelain_entries_do_not_claim_a_change() {
    let tmp = TempDir::new().expect("the new fixture result is required for this assertion");
    let run_git = |args: &[&str]| {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(tmp.path())
            .status()
            .expect("the status fixture result is required for this assertion")
            .success());
    };
    run_git(&[
        "-c",
        "user.name=Peri Test",
        "-c",
        "user.email=peri-test@example.invalid",
        "init",
        "-q",
    ]);
    std::fs::write(tmp.path().join("tracked.txt"), "base\n")
        .expect("the test fixture file must be writable");
    run_git(&["add", "tracked.txt"]);
    run_git(&[
        "-c",
        "user.name=Peri Test",
        "-c",
        "user.email=peri-test@example.invalid",
        "commit",
        "-qm",
        "baseline",
    ]);
    run_git(&["checkout", "-q", "-b", "side"]);
    std::fs::write(tmp.path().join("tracked.txt"), "side\n")
        .expect("the test fixture file must be writable");
    run_git(&[
        "-c",
        "user.name=Peri Test",
        "-c",
        "user.email=peri-test@example.invalid",
        "commit",
        "-qam",
        "side",
    ]);
    run_git(&["checkout", "-q", "-"]);
    std::fs::write(tmp.path().join("tracked.txt"), "main\n")
        .expect("the test fixture file must be writable");
    run_git(&[
        "-c",
        "user.name=Peri Test",
        "-c",
        "user.email=peri-test@example.invalid",
        "commit",
        "-qam",
        "main",
    ]);
    let merge = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=Peri Test",
            "-c",
            "user.email=peri-test@example.invalid",
            "merge",
            "--no-ff",
            "side",
        ])
        .current_dir(tmp.path())
        .status()
        .expect("the merge fixture result is required for this assertion");
    assert!(!merge.success(), "fixture 必须进入未合并状态");
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain=v2", "-z", "--untracked-files=all"])
        .current_dir(tmp.path())
        .output()
        .expect("the status fixture result is required for this assertion");
    let status =
        String::from_utf8(status.stdout).expect("the status fixture output must be valid UTF-8");
    assert!(
        status.split('\0').any(|field| field.starts_with("u ")),
        "fixture 必须产生未合并记录: {status}"
    );

    let baseline = GitBaseline::capture(tmp.path())
        .expect("the capture fixture result is required for this assertion");
    baseline
        .verify_unchanged()
        .expect("未合并条目本身不是运行带来的变化");
}

#[test]
fn workflow_journal_is_excluded_but_unrelated_untracked_files_are_not() {
    let tmp = TempDir::new().expect("the new fixture result is required for this assertion");
    let run_git = |args: &[&str]| {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(tmp.path())
            .status()
            .expect("the status fixture result is required for this assertion")
            .success());
    };
    run_git(&["init", "-q"]);
    std::fs::write(tmp.path().join("tracked.txt"), "base\n")
        .expect("the test fixture file must be writable");
    // 刻意不忽略 `.claude/`：宿主自己的运行产物不得被当成外部变化。
    run_git(&["add", "tracked.txt"]);
    run_git(&[
        "-c",
        "user.name=Peri Test",
        "-c",
        "user.email=peri-test@example.invalid",
        "commit",
        "-qm",
        "baseline",
    ]);

    let baseline = GitBaseline::capture(tmp.path())
        .expect("the capture fixture result is required for this assertion");
    let run_dir = tmp
        .path()
        .join(".claude")
        .join("workflow-runs")
        .join("run-1");
    std::fs::create_dir_all(&run_dir).expect("the run directory must be creatable");
    std::fs::write(run_dir.join("script.js"), "return null")
        .expect("the test fixture file must be writable");
    baseline
        .verify_unchanged()
        .expect("本 run 自己的 journal 不构成外部变化");

    std::fs::write(tmp.path().join("notes.txt"), "external\n")
        .expect("the test fixture file must be writable");
    let failure = baseline
        .verify_unchanged()
        .expect_err("journal 之外的未跟踪文件仍必须报告");
    assert!(
        matches!(
            failure,
            crate::journal::PostconditionFailure::Unattributed(_)
        ),
        "无 write footprint 时变化只能判为未归因: {failure}"
    );
    assert!(failure.to_string().contains("notes.txt"));
}

include!("journal_test_section_2.rs");
include!("journal_test_section_3.rs");
