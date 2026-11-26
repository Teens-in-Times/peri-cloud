// ─── run 索引排序与观察者锁 ─────────────────────────────────

#[test]
fn run_index_is_ordered_by_start_time_not_hashmap_iteration() {
    let store = make_store();
    for (run_id, started_at_ms) in [("r_late", 200i64), ("r_early_b", 100), ("r_early_a", 100)] {
        store.apply_event(&ProgressEvent::RunStarted {
            run_id: run_id.into(),
            workflow_name: "w".into(),
            meta: None,
        });
        store
            .runs
            .write()
            .get_mut(run_id)
            .expect("run 已登记")
            .progress
            .started_at_ms = started_at_ms;
    }

    let runs = store.list_runs();
    let order: Vec<&str> = runs.iter().map(|run| run.run_id.as_str()).collect();
    assert_eq!(
        order,
        vec!["r_early_a", "r_early_b", "r_late"],
        "升序（旧→新），同毫秒按 run_id 定序，不得依赖 HashMap 迭代顺序"
    );
    assert_eq!(
        store.list_runs().last().map(|run| run.run_id.as_str()),
        Some("r_late"),
        "「最近 N 个 run」= 尾部 N（WorkflowStatus 依赖该约定）"
    );
    let snapshot: Vec<String> = store
        .runs_snapshot()
        .into_iter()
        .map(|run| run.run_id)
        .collect();
    assert_eq!(snapshot, vec!["r_early_a", "r_early_b", "r_late"]);
}

#[test]
fn run_started_records_wall_clock_start_time() {
    let store = make_store();
    let before = chrono::Utc::now().timestamp_millis();
    store.apply_event(&ProgressEvent::RunStarted {
        run_id: "r1".into(),
        workflow_name: "w".into(),
        meta: None,
    });
    let after = chrono::Utc::now().timestamp_millis();
    let started = store
        .run_snapshot("r1")
        .expect("the run snapshot fixture result is required for this assertion")
        .started_at_ms;
    assert!(
        (before..=after).contains(&started),
        "started_at_ms 应落在本次调用窗口内，实际 {started}"
    );
}

/// 观察者回调期间不得持有 observer 读锁（否则回调内改注入会自锁）。
struct LockProbe {
    store: std::sync::Weak<WorkflowProgressStore>,
    calls: std::sync::atomic::AtomicUsize,
}

impl ProgressObserver for LockProbe {
    fn on_progress_event(&self, _event: &ProgressEvent) {
        let store = self.store.upgrade().expect("store 存活");
        assert!(
            store.observer.try_write().is_some(),
            "observer 回调不得持有读锁"
        );
        drop(store);
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[test]
fn progress_observer_is_called_without_holding_the_observer_lock() {
    let store = Arc::new(WorkflowProgressStore::new());
    let probe = Arc::new(LockProbe {
        store: Arc::downgrade(&store),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    store.set_observer(probe.clone());
    store.apply_event(&ProgressEvent::Log {
        run_id: "r1".into(),
        message: "hello".into(),
    });
    assert_eq!(probe.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// 记录 `RunDone` 回调时刻 store 中该 run 的快照（用于断言投影已可见）。
struct ProjectionProbe {
    store: std::sync::Weak<WorkflowProgressStore>,
    seen: parking_lot::Mutex<Option<RunProgress>>,
}

impl ProgressObserver for ProjectionProbe {
    fn on_progress_event(&self, event: &ProgressEvent) {
        if matches!(event, ProgressEvent::RunDone { .. }) {
            let store = self.store.upgrade().expect("store 存活");
            *self.seen.lock() = store.run_snapshot(event.run_id());
        }
    }
}

#[test]
fn run_done_projection_is_committed_before_the_observer_is_notified() {
    let store = Arc::new(WorkflowProgressStore::new());
    let probe = Arc::new(ProjectionProbe {
        store: Arc::downgrade(&store),
        seen: parking_lot::Mutex::new(None),
    });
    store.set_observer(probe.clone());
    store.apply_event(&ProgressEvent::RunStarted {
        run_id: "r1".into(),
        workflow_name: "w".into(),
        meta: None,
    });
    store.apply_run_done(
        "r1",
        "completed",
        None,
        None,
        peri_acp_types::workflow::PostProcessingStatus::Failed,
        peri_acp_types::workflow::DeliveryStatus::Blocked,
        false,
    );

    let seen = probe.seen.lock().clone().expect("RunDone 已回调");
    assert!(
        matches!(seen.status, RunStatus::Completed),
        "回调时刻终态已提交"
    );
    assert_eq!(
        seen.delivery_status,
        peri_acp_types::workflow::DeliveryStatus::Blocked,
        "投影与 RunDone 同一次提交，通知不得早于投影"
    );
    assert_eq!(
        seen.post_processing_status,
        peri_acp_types::workflow::PostProcessingStatus::Failed
    );
}

#[test]
fn unknown_side_effects_is_projected_only_when_set() {
    let store = make_store();
    store.apply_event(&ProgressEvent::RunStarted {
        run_id: "r1".into(),
        workflow_name: "w".into(),
        meta: None,
    });
    store.apply_run_done(
        "r1",
        "killed",
        None,
        Some("killed".into()),
        peri_acp_types::workflow::PostProcessingStatus::Blocked,
        peri_acp_types::workflow::DeliveryStatus::Blocked,
        true,
    );

    let run = store.run_snapshot("r1").expect("run 应存在");
    assert!(run.unknown_side_effects);
    let json = serde_json::to_value(&run).expect("RunProgress 必须可序列化");
    assert_eq!(json["unknown_side_effects"], true);

    let quiet = make_store();
    quiet.apply_event(&ProgressEvent::RunStarted {
        run_id: "r2".into(),
        workflow_name: "w".into(),
        meta: None,
    });
    quiet.apply_run_done(
        "r2",
        "completed",
        None,
        None,
        peri_acp_types::workflow::PostProcessingStatus::Unknown,
        peri_acp_types::workflow::DeliveryStatus::Unknown,
        false,
    );
    let json = serde_json::to_value(quiet.run_snapshot("r2").expect("run 应存在"))
        .expect("RunProgress 必须可序列化");
    assert!(
        json.get("unknown_side_effects").is_none(),
        "false 不得落 JSON（与 state.json 的 true 才出现一致）"
    );
}
