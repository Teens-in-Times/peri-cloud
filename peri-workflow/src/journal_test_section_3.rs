#[test]
fn git_baseline_records_repository_identity_and_visible_scope() {
    let tmp = TempDir::new().expect("the temporary test directory must be available");
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
    let canonical = tmp
        .path()
        .canonicalize()
        .expect("the temporary directory must be canonicalizable");

    let baseline = GitBaseline::capture(tmp.path())
        .expect("the capture fixture result is required for this assertion");
    assert_eq!(baseline.git_dir, canonical.join(".git"));
    assert_eq!(baseline.git_common_dir, canonical.join(".git"));

    let scope = baseline.observation_scope();
    assert_eq!(scope.repo_root, canonical.to_string_lossy());
    assert_eq!(scope.cwd, canonical.to_string_lossy());
    assert_eq!(scope.git_dir, canonical.join(".git").to_string_lossy());
    assert_eq!(scope.head, baseline.head);
    assert!(scope.visibility.index);
    assert!(scope.visibility.worktree);
    assert!(
        !scope.visibility.ignored,
        "ignored 文件不进入基线，其变化不可观测"
    );
    assert!(
        !scope.visibility.commit_range,
        "只比较运行前后 HEAD 端点，不覆盖完整提交区间"
    );
}

#[test]
fn postcondition_failure_messages_carry_the_attribution_tier() {
    use crate::journal::PostconditionFailure;

    for (failure, message, display) in [
        (
            PostconditionFailure::Violation("escaped writeIntent.path_allowlist".into()),
            "escaped writeIntent.path_allowlist",
            "run-owned violation: escaped writeIntent.path_allowlist",
        ),
        (
            PostconditionFailure::Unattributed("git HEAD changed".into()),
            "git HEAD changed",
            "unattributed change (external or unknown): git HEAD changed",
        ),
        (
            PostconditionFailure::ObservationIncomplete("cannot sample".into()),
            "cannot sample",
            "observation incomplete: cannot sample",
        ),
    ] {
        assert_eq!(failure.to_string(), display);
        assert_eq!(
            failure.message(),
            message,
            "结构化 message 保持不带档位前缀"
        );
    }
}

/// 观测窗口被重新绑定（cwd 指向另一个 git dir）时不能对比旧基线宣称“无变化”。
#[cfg(unix)]
#[test]
fn rebinding_the_git_directory_is_incomplete_not_an_unattributed_change() {
    use crate::journal::PostconditionFailure;

    let tmp = TempDir::new().expect("the temporary test directory must be available");
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).expect("the test fixture directory must be creatable");
    let run_git = |args: &[&str]| {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .status()
            .expect("the status fixture result is required for this assertion")
            .success());
    };
    run_git(&["init", "-q"]);
    std::fs::write(repo.join("tracked.txt"), "base\n")
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
    let baseline = GitBaseline::capture(&repo)
        .expect("the capture fixture result is required for this assertion");

    let moved_git = tmp.path().join("moved-git");
    std::fs::rename(repo.join(".git"), &moved_git)
        .expect("the test fixture git directory must be movable");
    std::os::unix::fs::symlink(&moved_git, repo.join(".git"))
        .expect("the test fixture symlink must be creatable");

    let failure = baseline
        .verify_unchanged()
        .expect_err("重新绑定 git dir 后不得宣称无变化");
    assert!(
        matches!(failure, PostconditionFailure::ObservationIncomplete(_)),
        "观测窗口切换属于无法观测，不是外部变化: {failure}"
    );
    assert!(failure.to_string().starts_with("observation incomplete: "));
}

/// 旧 `state.json` 没有限额明细：缺省为空/None，不得据此推断“未触发过限额”。
#[test]
fn test_legacy_state_without_limit_breaches_deserializes_as_unknown() {
    let state: RunState = serde_json::from_value(serde_json::json!({
        "run_id": "legacy-limits",
        "workflow_name": "legacy",
        "status": "failed",
        "script": "script",
        "started_at": "2026-11-21T00:00:00Z",
        "error": "workflow exceeded maxToolCalls (5)"
    }))
    .expect("from_value must accept this test fixture representation");

    assert!(state.limit_breaches.is_empty());
    assert_eq!(state.token_usage, None, "旧数据未采集用量，不显示为 0");
    assert!(
        !state.unknown_side_effects,
        "旧 state.json 缺 unknownSideEffects 时按 false 读取（不得据此推断本能力已存在）"
    );
    assert_eq!(
        state.git_observation_scope, None,
        "旧数据未记录 Git 观测范围，不得推断为完整观测"
    );
}

/// `unknownSideEffects` 只在为 `true` 时写出（camelCase）；默认 false 不落盘。
#[test]
fn unknown_side_effects_is_serialized_only_when_true() {
    let mut state: RunState = serde_json::from_value(serde_json::json!({
        "run_id": "side-effects",
        "workflow_name": "side-effects",
        "status": "completed",
        "script": "script",
        "started_at": "2026-11-22T00:00:00Z"
    }))
    .expect("from_value must accept this test fixture representation");
    let quiet = serde_json::to_value(&state).expect("state 可序列化");
    assert!(
        quiet.get("unknownSideEffects").is_none(),
        "false 不写出该字段：{quiet}"
    );

    state.unknown_side_effects = true;
    let flagged = serde_json::to_value(&state).expect("state 可序列化");
    assert_eq!(flagged["unknownSideEffects"], true);
}

/// 终态只保存观测身份与能力：camelCase 范围字段可读，基线内容（porcelain）不落盘。
#[test]
fn run_state_persists_git_observation_scope_without_baseline_content() {
    let state: RunState = serde_json::from_value(serde_json::json!({
        "run_id": "scope-run",
        "workflow_name": "scope",
        "status": "completed",
        "script": "script",
        "started_at": "2026-11-21T00:00:00Z",
        "git_observation_scope": {
            "repoRoot": "/repo",
            "cwd": "/repo/sub",
            "gitDir": "/repo/.git",
            "gitCommonDir": "/repo/.git",
            "head": "0123456789abcdef",
            "visibility": {
                "index": true,
                "worktree": true,
                "ignored": false,
                "commitRange": false
            }
        }
    }))
    .expect("from_value must accept git_observation_scope");

    let scope = state
        .git_observation_scope
        .as_ref()
        .expect("记录的范围必须可读");
    assert_eq!(scope.git_dir, "/repo/.git");
    assert_eq!(scope.git_common_dir, "/repo/.git");
    assert!(scope.visibility.index && scope.visibility.worktree);
    assert!(!scope.visibility.ignored && !scope.visibility.commit_range);

    let wire = serde_json::to_value(&state).expect("state must serialize");
    assert_eq!(wire["git_observation_scope"]["gitCommonDir"], "/repo/.git");
    assert!(
        wire.get("status_porcelain_v2").is_none()
            && wire["git_observation_scope"]
                .get("status_porcelain_v2")
                .is_none(),
        "终态不得持久化 baseline 内容"
    );
}

/// 新 state.json 的限额条目按 camelCase wire 写出，未采集字段缺席。
#[test]
fn test_limit_breaches_and_token_usage_serialize_without_zero_fillers() {
    let state = RunState {
        run_id: "run-limits".into(),
        workflow_name: "limits".into(),
        status: "failed".into(),
        execution_status: Default::default(),
        acceptance_status: Default::default(),
        post_processing_status: Default::default(),
        delivery_status: Default::default(),
        delivery_reason: None,
        postcondition_observation: None,
        git_observation_scope: None,
        outcome: Default::default(),
        unknown_side_effects: false,
        write_intent: None,
        limits: crate::protocol::WorkflowLimits {
            max_tool_calls: Some(5),
            ..Default::default()
        },
        limit_breaches: vec![peri_acp_types::workflow::LimitBreach {
            scope: peri_acp_types::workflow::LimitScope::Iterations,
            limit: Some(400),
            observed: None,
            attempt: Some(peri_acp_types::workflow::LimitBreachAttempt {
                run_id: "run-limits".into(),
                task_id: Some("task-1".into()),
                agent_id: 3,
            }),
            detected_at: "2026-11-21T00:00:00Z".into(),
        }],
        token_usage: Some(75_323),
        budget_total: Some(60_000),
        args: None,
        max_concurrency: 3,
        attempts: Vec::new(),
        return_value: None,
        script: "script".into(),
        started_at: "2026-11-21T00:00:00Z".into(),
        finished_at: None,
        error: Some("workflow exceeded maxIterations (400)".into()),
    };
    let wire = serde_json::to_value(&state).expect("state must serialize");
    assert_eq!(wire["limits"]["maxToolCalls"], 5);
    assert_eq!(wire["limit_breaches"][0]["scope"], "iterations");
    assert_eq!(wire["limit_breaches"][0]["limit"], 400);
    assert!(
        wire["limit_breaches"][0].get("observed").is_none(),
        "未采集用量不得写成 0"
    );
    assert_eq!(wire["token_usage"], 75_323);
    assert_eq!(wire["budget_total"], 60_000);
}
