/// [回归测试] 终态 run 退役后必须回收事件水位与 run/agent 取消栅栏（#468）。
///
/// 隔离运行结果：8 个 run 完成后把清理时钟推进 301 秒，可见 run 归零、环形日志
/// 只剩保留窗口内的条目，但仍能按 8/8 个退役 run ID 查到水位。
#[test]
fn retired_runs_release_event_watermarks_and_cancel_fences() {
    let dir = tempfile::tempdir().expect("the temporary test directory must be available");
    let runtime = make_runtime(
        dir.path()
            .to_str()
            .expect("the fixture text must be valid UTF-8"),
    );

    let run_count: u64 = 8;
    let mut completed_at = None;
    for index in 0..run_count {
        let run_id = format!("run-{index}");
        apply_run_started(&runtime, &run_id);
        // Log 也会进入 run 事件日志（水位前进），但不改变 run 可读状态。
        runtime
            .progress_store()
            .apply_event(&crate::protocol::ProgressEvent::Log {
                run_id: run_id.clone(),
                message: "noise".into(),
            });
        let _ = runtime
            .tasks()
            .publish(format!("t-{index}"), task_params(&run_id, index));
        runtime
            .tasks()
            .cancel_run(&run_id, Some("workflow run killed".into()));
        runtime
            .tasks()
            .cancel_agent(&run_id, index + 1000, Some("agent killed".into()));
        apply_run_done(&runtime, &run_id, "killed");
        let snapshot = runtime
            .progress_store()
            .run_snapshot(&run_id)
            .expect("the fixture run must be visible before retirement");
        completed_at = snapshot.completed_at;
        assert!(
            runtime.event_watermark(&run_id) > 0,
            "可见 run 的已提交事件必须留下水位"
        );
        assert_eq!(
            publish_state(&runtime, &run_id, index + 100, "before"),
            Some(crate::runtime::WorkflowTaskState::Cancelled),
            "退役前迟到任务仍受 run 级取消栅栏约束"
        );
        assert_eq!(
            publish_state(&runtime, &run_id, index + 1000, "before"),
            Some(crate::runtime::WorkflowTaskState::Cancelled),
            "退役前迟到任务仍受 agent 级取消栅栏约束"
        );
        // 生产者的终态收敛点（runner 返回 + completion 投影完成）：run 仍可见。
        runtime.progress_store().note_producer_finished(&run_id);
    }

    // 退役前：水位仍单调可读（清理时钟未过保留窗口时不得回收）。
    let completed_at = completed_at.expect("the fixture run must be terminal");
    runtime
        .progress_store()
        .cleanup_completed_at(completed_at + std::time::Duration::from_secs(1));
    assert_eq!(
        runtime.progress_store().list_runs().len(),
        run_count as usize
    );
    for index in 0..run_count {
        assert!(
            runtime.event_watermark(&format!("run-{index}")) > 0,
            "可见 run 的水位不得因清理尝试而回退或消失"
        );
    }

    // 退役：清理时钟推进 301 秒（超过 300 秒保留窗口）。
    runtime
        .progress_store()
        .cleanup_completed_at(completed_at + std::time::Duration::from_secs(301));
    assert!(
        runtime.progress_store().list_runs().is_empty(),
        "终态 run 必须按保留窗口退役"
    );
    for index in 0..run_count {
        let run_id = format!("run-{index}");
        assert_eq!(
            runtime.event_watermark(&run_id),
            0,
            "退役 run 的水位必须随 run 一起回收"
        );
        assert_eq!(
            publish_state(&runtime, &run_id, index + 100, "after"),
            Some(crate::runtime::WorkflowTaskState::Published),
            "退役后不再有生产者，run 级取消栅栏不得继续驻留"
        );
        assert_eq!(
            publish_state(&runtime, &run_id, index + 1000, "after"),
            Some(crate::runtime::WorkflowTaskState::Published),
            "退役后不再有生产者，agent 级取消栅栏不得继续驻留"
        );
    }
}

/// [回归测试] 回收必须等 run 退役与生产者结束同时成立（#468）：退役但生产者
/// 仍可能发布时，水位与取消栅栏都不得提前释放。
#[test]
fn retirement_without_producer_end_keeps_watermark_and_cancel_fence() {
    let dir = tempfile::tempdir().expect("the temporary test directory must be available");
    let runtime = make_runtime(
        dir.path()
            .to_str()
            .expect("the fixture text must be valid UTF-8"),
    );
    let run_id = "run-live-producer";
    apply_run_started(&runtime, run_id);
    runtime
        .progress_store()
        .apply_event(&crate::protocol::ProgressEvent::Log {
            run_id: run_id.to_string(),
            message: "noise".into(),
        });
    let _ = runtime
        .tasks()
        .publish("t-live".into(), task_params(run_id, 1));
    runtime
        .tasks()
        .cancel_run(run_id, Some("workflow run killed".into()));
    apply_run_done(&runtime, run_id, "killed");
    let completed_at = runtime
        .progress_store()
        .run_snapshot(run_id)
        .and_then(|snapshot| snapshot.completed_at)
        .expect("the fixture run must be terminal");

    // 清理未过保留窗口：run 仍可见，水位保持。
    runtime
        .progress_store()
        .cleanup_completed_at(completed_at + std::time::Duration::from_secs(1));
    assert_eq!(runtime.progress_store().list_runs().len(), 1);
    assert!(runtime.event_watermark(run_id) > 0);

    // 退役但生产者未结束：水位与栅栏继续驻留（迟到任务仍受取消约束）。
    runtime
        .progress_store()
        .cleanup_completed_at(completed_at + std::time::Duration::from_secs(301));
    assert!(runtime.progress_store().list_runs().is_empty());
    assert!(
        runtime.event_watermark(run_id) > 0,
        "生产者未结束前不得回收水位"
    );
    assert_eq!(
        publish_state(&runtime, run_id, 7, "before-producer-end"),
        Some(crate::runtime::WorkflowTaskState::Cancelled),
        "生产者未结束前迟到任务仍受取消约束"
    );

    // 生产者结束：立即回收已退役 run 的水位与栅栏。
    runtime.progress_store().note_producer_finished(run_id);
    assert_eq!(runtime.event_watermark(run_id), 0);
    assert_eq!(
        publish_state(&runtime, run_id, 7, "after-producer-end"),
        Some(crate::runtime::WorkflowTaskState::Published),
        "生产者结束后不再有迟到发布，栅栏可以回收"
    );
}

/// [回归测试] 最后一个 run 结束后没有后续 workflow 工作：实例必须按最近的终态
/// 到期时间自行唤醒清扫，释放含完整输出的进度快照（内存审查 P1-B）。
#[tokio::test(start_paused = true)]
async fn idle_instance_releases_terminal_run_at_retention_expiry() {
    let dir = tempfile::tempdir().expect("the temporary test directory must be available");
    let runtime = make_runtime(
        dir.path()
            .to_str()
            .expect("the fixture text must be valid UTF-8"),
    );
    let run_id = "run-idle";
    apply_run_started(&runtime, run_id);
    apply_run_done(&runtime, run_id, "completed");

    // 未到期：完整结果仍可按产品契约查询。
    advance_sweeper_clock(std::time::Duration::from_secs(299)).await;
    assert!(
        runtime.run_snapshot(run_id).is_some(),
        "五分钟保留窗口内的终态快照必须可查"
    );

    // 到期：即使没有下一次 workflow 工作，快照也必须释放。
    advance_sweeper_clock(std::time::Duration::from_secs(2)).await;
    assert!(
        runtime.run_snapshot(run_id).is_none(),
        "空闲实例必须按到期释放最后一批完整结果（P1-B）"
    );
}

/// [回归测试] 新 run 到达重置清扫定时器：到期时间按当前终态重新计算，运行中的
/// run 不被旧截止时刻提前淘汰（内存审查 P1-B）。
#[tokio::test(start_paused = true)]
async fn new_run_resets_expiry_deadline() {
    let dir = tempfile::tempdir().expect("the temporary test directory must be available");
    let runtime = make_runtime(
        dir.path()
            .to_str()
            .expect("the fixture text must be valid UTF-8"),
    );
    apply_run_started(&runtime, "run-a");
    apply_run_done(&runtime, "run-a", "completed");

    // 新 run 在 run-a 的保留窗口内到达：定时器按当前状态重置。
    advance_sweeper_clock(std::time::Duration::from_secs(120)).await;
    apply_run_started(&runtime, "run-b");

    // run-a 到期：只退役自身；运行中的 run-b 继续可见。
    advance_sweeper_clock(std::time::Duration::from_secs(181)).await;
    assert!(
        runtime.run_snapshot("run-a").is_none(),
        "最早的终态到期时刻必须仍被唤醒清扫"
    );
    assert!(
        runtime.run_snapshot("run-b").is_some(),
        "运行中的 run 不得被上一次的到期时刻淘汰"
    );

    // run-b 结束后按它自己的完成时刻重新开窗。
    apply_run_done(&runtime, "run-b", "completed");
    advance_sweeper_clock(std::time::Duration::from_secs(299)).await;
    assert!(
        runtime.run_snapshot("run-b").is_some(),
        "新终态的保留窗口从自身完成时刻起算"
    );
    advance_sweeper_clock(std::time::Duration::from_secs(2)).await;
    assert!(
        runtime.run_snapshot("run-b").is_none(),
        "新终态同样按到期释放"
    );
}

/// [回归测试] 实例关闭撤销到期唤醒：关闭后不再有定时清扫（内存审查 P1-B）。
#[tokio::test(start_paused = true)]
async fn close_cancels_expiry_sweep() {
    let dir = tempfile::tempdir().expect("the temporary test directory must be available");
    let runtime = make_runtime(
        dir.path()
            .to_str()
            .expect("the fixture text must be valid UTF-8"),
    );
    let run_id = "run-closed";
    apply_run_started(&runtime, run_id);
    apply_run_done(&runtime, run_id, "completed");

    runtime.mark_closed();
    // 关闭后迟到的终态事件也不得重新登记唤醒。
    apply_run_done(&runtime, run_id, "completed");

    advance_sweeper_clock(std::time::Duration::from_secs(301)).await;
    assert!(
        runtime.run_snapshot(run_id).is_some(),
        "实例关闭后清扫唤醒必须撤销"
    );
}

/// [回归测试] 到期清扫只退役进度快照：未 ACK 的完成通知、迟到事件的取消栅栏与
/// 事件水位仍按既有保留窗口和生产者结算语义保留（内存审查 P1-B）。
#[tokio::test(start_paused = true)]
async fn expiry_keeps_unacked_completion_and_cancel_fences() {
    use peri_acp_types::workflow::{
        AcceptanceStatus, DeliveryStatus, ExecutionStatus, PostProcessingStatus,
    };

    let dir = tempfile::tempdir().expect("the temporary test directory must be available");
    let runtime = make_runtime(
        dir.path()
            .to_str()
            .expect("the fixture text must be valid UTF-8"),
    );
    let run_id = "run-delivery";
    apply_run_started(&runtime, run_id);
    runtime
        .tasks()
        .cancel_run(run_id, Some("workflow run killed".into()));
    apply_run_done(&runtime, run_id, "killed");

    let ticket = runtime
        .registry()
        .reserve(crate::registry::WorkflowRun {
            run_id: run_id.into(),
            workflow_name: "t".into(),
            script_preview: "return 1".into(),
            status: crate::registry::WorkflowRunStatus::Running,
            started_at: std::time::Instant::now(),
            child_handle: None,
            kill_tx: None,
        })
        .expect("the run reservation fixture result is required for this assertion");
    assert!(runtime.registry().complete(
        &ticket,
        crate::registry::WorkflowTaskResult {
            run_id: run_id.into(),
            workflow_name: "t".into(),
            success: false,
            status: crate::registry::WorkflowRunStatus::Killed,
            execution_status: ExecutionStatus::Killed,
            acceptance_status: AcceptanceStatus::Unknown,
            post_processing_status: PostProcessingStatus::Unknown,
            delivery_status: DeliveryStatus::Blocked,
            outcome: Default::default(),
            unknown_side_effects: false,
            state_artifact_exists: false,
            duration_ms: 0,
            agent_count: 0,
            tool_calls_count: 0,
            error: Some("killed".into()),
            result_preview: None,
            phase_summaries: Vec::new(),
            attempts: Vec::new(),
        },
        peri_acp_types::tasks::TaskRecord {
            task_id: run_id.into(),
            kind: peri_acp_types::tasks::BgTaskKind::Workflow,
            summary: "t".into(),
            started_at: "2026-11-11T00:00:00Z".into(),
            status: "cancelled".into(),
            duration_ms: 0,
            output_preview: None,
            initiator_session_id: None,
            child_thread_id: None,
            external_owner: None,
        },
    ));

    // 窗口内：迟到任务仍受取消栅栏约束，完成通知等待 ACK。
    advance_sweeper_clock(std::time::Duration::from_secs(299)).await;
    assert_eq!(
        publish_state(&runtime, run_id, 21, "in-window"),
        Some(crate::runtime::WorkflowTaskState::Cancelled),
        "可见 run 的取消栅栏不得被清扫提前释放"
    );
    assert_eq!(runtime.registry().pending_completions().len(), 1);

    // 到期：只退役进度快照；生产者未结算时水位与栅栏继续驻留。
    advance_sweeper_clock(std::time::Duration::from_secs(2)).await;
    assert!(runtime.run_snapshot(run_id).is_none());
    assert!(
        runtime.event_watermark(run_id) > 0,
        "生产者未结束前事件水位不得回收"
    );
    assert_eq!(
        publish_state(&runtime, run_id, 22, "after-expiry"),
        Some(crate::runtime::WorkflowTaskState::Cancelled),
        "生产者未结束前迟到任务仍受取消约束"
    );
    assert_eq!(
        runtime.registry().pending_completions().len(),
        1,
        "未 ACK 的完成通知不随进度快照退役"
    );

    // 生产者结束：既有回收语义不变。
    runtime.progress_store().note_producer_finished(run_id);
    assert_eq!(runtime.event_watermark(run_id), 0);
    assert_eq!(
        publish_state(&runtime, run_id, 23, "after-producer"),
        Some(crate::runtime::WorkflowTaskState::Published)
    );
    assert_eq!(
        runtime.registry().pending_completions().len(),
        1,
        "完成通知仍等宿主 ACK"
    );
}

/// 推进暂停的 tokio 时钟，并给到期清扫任务一次运行机会。
async fn advance_sweeper_clock(duration: std::time::Duration) {
    tokio::time::advance(duration).await;
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

fn apply_run_started(runtime: &WorkflowRuntime, run_id: &str) {
    runtime
        .progress_store()
        .apply_event(&crate::protocol::ProgressEvent::RunStarted {
            run_id: run_id.to_string(),
            workflow_name: "t".into(),
            meta: None,
        });
}

fn apply_run_done(runtime: &WorkflowRuntime, run_id: &str, status: &str) {
    runtime
        .progress_store()
        .apply_event(&crate::protocol::ProgressEvent::RunDone {
            run_id: run_id.to_string(),
            status: status.to_string(),
            return_value: None,
            error: None,
        });
}

/// 发布一个迟到任务并返回其提交后的状态（不存在时 `None`）。
fn publish_state(
    runtime: &WorkflowRuntime,
    run_id: &str,
    agent_id: u64,
    phase: &str,
) -> Option<crate::runtime::WorkflowTaskState> {
    let task_id = format!("{run_id}-probe-{agent_id}-{phase}");
    match runtime
        .tasks()
        .publish(task_id, task_params(run_id, agent_id))
    {
        crate::runtime::PublishOutcome::Published(record) => Some(record.state),
        crate::runtime::PublishOutcome::AlreadyKnown(record) => Some(record.state),
        crate::runtime::PublishOutcome::Closed => None,
    }
}
