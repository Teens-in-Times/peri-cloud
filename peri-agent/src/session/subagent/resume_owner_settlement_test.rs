/// [回归测试] 交接确认后 history flush 失败，不能放行同步执行。
#[tokio::test(start_paused = true)]
async fn test_sync_post_handoff_history_flush_failure_blocks_resume() {
    test_post_handoff_history_flush_failure(false).await;
}

/// [回归测试] 交接确认后 history flush 失败，不能放行后台执行。
#[tokio::test(start_paused = true)]
async fn test_background_post_handoff_history_flush_failure_blocks_resume() {
    test_post_handoff_history_flush_failure(true).await;
}

#[derive(Clone)]
struct FailWithoutResidual;

impl FailWithoutResidual {
    async fn respond(
        &self,
        _request: peri_model::ModelRequest,
        _cancellation: CancellationToken,
    ) -> Vec<peri_model::ModelResult<peri_model::ModelStreamEvent>> {
        vec![Err(peri_model::ModelError::http_status(
            400,
            "fixture",
            None::<&str>,
        ))]
    }
}
crate::fixture_model_impl!(FailWithoutResidual);

async fn test_failed_without_receipt_resume(background: bool, late_obligations: bool) {
    use crate::session::subagent::{ParkedChildOwner, SubagentFailure};
    use crate::session::{FrozenContext, MessageQueue, Session};
    use peri_acp_types::tasks::{BgTaskKind, BgTaskRegistration, TaskManager as _};
    use std::time::Duration;

    let store = MockSessionResources::new();
    let child_id = uuid::Uuid::now_v7().to_string();
    let mut meta = ThreadMeta::new_at("/tmp", peri_time::now_wall());
    meta.id = child_id.clone();
    meta.parent_thread_id = Some("parent-thread".into());
    store.create_resumable_thread(meta).await.unwrap();
    store
        .update_thread_status(&child_id, "error")
        .await
        .unwrap();
    let parent_manager = Arc::new(TaskManager::new());
    let child_manager = Arc::new(TaskManager::new());
    let owner = ParkedChildOwner::new(
        child_id.clone(),
        "parent-thread".into(),
        child_manager.clone(),
        MessageQueue::new(),
        Some(store.clone()),
        None,
    );
    parent_manager.register_child_owner(owner.clone()).unwrap();
    let parent = Session::new(
        Arc::from("/tmp"),
        FrozenContext::builder().build(),
        Some("parent-thread".into()),
    );
    parent.set_subagent_host(SubagentHost {
        task_manager: Some(parent_manager.clone()),
        ..Default::default()
    });
    let mut config = resume_config_with(
        store.clone(),
        child_id.clone(),
        crate::session::test_resources::mock::model::fixture_source(
            Arc::new(FailWithoutResidual),
            "fixture-scripted",
        ),
        if background {
            SubagentRunMode::Background
        } else {
            SubagentRunMode::Sync
        },
        Some(parent_manager),
        None,
    );
    if late_obligations {
        let late_manager = child_manager.clone();
        let late_queue = owner.queue.clone();
        config.on_subagent_stop = Some(Arc::new(move |_, _, _, _, _| {
            late_manager
                .register(BgTaskRegistration {
                    task_id: "late-shell".into(),
                    kind: BgTaskKind::Shell,
                    summary: "结算期间新到达的任务".into(),
                    pid: None,
                    kill: Some(Box::new(|| {})),
                })
                .unwrap();
            late_queue.push_sync(crate::session::QueuedMessage::prompt(
                crate::session::MessageSource::UserInput,
                BaseMessage::human("结算期间到达的输入"),
            ));
        }));
    }
    let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
    let finished_tx = parking_lot::Mutex::new(Some(finished_tx));
    config.deregister_runtime = Some(Arc::new(move |_| {
        if let Some(sender) = finished_tx.lock().take() {
            let _ = sender.send(());
        }
    }));
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let result_tx = parking_lot::Mutex::new(Some(result_tx));
    config.on_bg_complete = Some(Arc::new(move |result, _| {
        if let Some(sender) = result_tx.lock().take() {
            let _ = sender.send(result.clone());
        }
        Ok(())
    }));
    let first = SessionFactory::resume_subagent(Some(&parent), config).await;
    if background {
        first.expect("后台执行须成功启动");
        let result = tokio::time::timeout(Duration::from_secs(10), result_rx)
            .await
            .expect("后台失败必须完成")
            .unwrap();
        assert!(!result.success);
    } else {
        let error = first.err().expect("模型失败必须返回错误");
        let failure = error.downcast_ref::<SubagentFailure>().unwrap();
        assert!(
            failure.handoff().is_none(),
            "模型失败时尚无义务，不能伪造交接"
        );
    }
    tokio::time::timeout(Duration::from_secs(10), finished_rx)
        .await
        .expect("旧执行必须完成")
        .unwrap();
    assert!(owner.receipt().is_none());
    if !late_obligations {
        assert!(owner.failure().is_none());
        assert_eq!(child_manager.active_count(), 0);
        let resumed =
            SessionFactory::resume_subagent(Some(&parent), resume_config(store, child_id.clone()))
                .await
                .expect("没有残余义务的失败应允许新执行");
        assert_eq!(resumed.child_thread_id, child_id);
        return;
    }
    assert_eq!(child_manager.active_count(), 1);
    assert!(owner.queue.has_required_sync());
    assert_eq!(
        owner.failure().as_deref(),
        Some("child settlement is unconfirmed after execution failure")
    );
    let error = SessionFactory::resume_subagent(Some(&parent), resume_config(store, child_id))
        .await
        .err()
        .expect("无交接回执的未确认义务必须阻止 resume")
        .to_string();
    assert!(error.contains("settlement is unconfirmed"), "{error}");
}

/// [回归测试] 无 receipt 的同步失败在结算期间出现新义务时必须阻断。
#[tokio::test(start_paused = true)]
async fn test_sync_failed_without_receipt_late_obligations_block_resume() {
    test_failed_without_receipt_resume(false, true).await;
}

/// [回归测试] 无 receipt 的后台失败在结算期间出现新义务时必须阻断。
#[tokio::test(start_paused = true)]
async fn test_background_failed_without_receipt_late_obligations_block_resume() {
    test_failed_without_receipt_resume(true, true).await;
}

/// [回归测试] 无 receipt 且无遗留义务的同步失败可以恢复。
#[tokio::test(start_paused = true)]
async fn test_sync_failed_without_receipt_or_obligations_can_resume() {
    test_failed_without_receipt_resume(false, false).await;
}

/// [回归测试] 无 receipt 且无遗留义务的后台失败可以恢复。
#[tokio::test(start_paused = true)]
async fn test_background_failed_without_receipt_or_obligations_can_resume() {
    test_failed_without_receipt_resume(true, false).await;
}

#[derive(Clone)]
struct FailWithUnwritableStore(Arc<MockSessionResources>);

impl FailWithUnwritableStore {
    async fn respond(
        &self,
        _request: peri_model::ModelRequest,
        _cancellation: CancellationToken,
    ) -> Vec<peri_model::ModelResult<peri_model::ModelStreamEvent>> {
        self.0.restrict_to_history_read_only();
        vec![Err(peri_model::ModelError::http_status(
            400,
            "fixture",
            None::<&str>,
        ))]
    }
}
crate::fixture_model_impl!(FailWithUnwritableStore);

async fn test_unconfirmed_handoff_blocks_factory_resume(background: bool) {
    use crate::session::subagent::ParkedChildOwner;
    use crate::session::{FrozenContext, MessageQueue, Session};
    use peri_acp_types::tasks::{BgTaskKind, BgTaskRegistration, TaskManager as _};
    use std::time::Duration;

    let store = MockSessionResources::new();
    let child_id = uuid::Uuid::now_v7().to_string();
    let mut meta = ThreadMeta::new_at("/tmp", peri_time::now_wall());
    meta.id = child_id.clone();
    meta.parent_thread_id = Some("parent-thread".into());
    store.create_resumable_thread(meta).await.unwrap();
    store
        .update_thread_status(&child_id, "error")
        .await
        .unwrap();
    let parent_manager = Arc::new(TaskManager::new());
    let child_manager = Arc::new(TaskManager::new());
    child_manager
        .register(BgTaskRegistration {
            task_id: "unconfirmed-shell".into(),
            kind: BgTaskKind::Shell,
            summary: "测试中的后台任务".into(),
            pid: None,
            kill: Some(Box::new(|| {})),
        })
        .unwrap();
    let owner = ParkedChildOwner::new(
        child_id.clone(),
        "parent-thread".into(),
        child_manager,
        MessageQueue::new(),
        Some(store.clone()),
        None,
    );
    parent_manager.register_child_owner(owner.clone()).unwrap();
    let parent = Session::new(
        Arc::from("/tmp"),
        FrozenContext::builder().build(),
        Some("parent-thread".into()),
    );
    parent.set_subagent_host(SubagentHost {
        task_manager: Some(parent_manager.clone()),
        ..Default::default()
    });
    let mut config = resume_config_with(
        store.clone(),
        child_id.clone(),
        crate::session::test_resources::mock::model::fixture_source(
            Arc::new(FailWithUnwritableStore(store.clone())),
            "fixture-scripted",
        ),
        if background {
            SubagentRunMode::Background
        } else {
            SubagentRunMode::Sync
        },
        Some(parent_manager),
        None,
    );
    let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
    let finished_tx = parking_lot::Mutex::new(Some(finished_tx));
    config.deregister_runtime = Some(Arc::new(move |_| {
        if let Some(sender) = finished_tx.lock().take() {
            let _ = sender.send(());
        }
    }));
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let result_tx = parking_lot::Mutex::new(Some(result_tx));
    config.on_bg_complete = Some(Arc::new(move |result, _| {
        if let Some(sender) = result_tx.lock().take() {
            let _ = sender.send(result.clone());
        }
        Ok(())
    }));
    let first = SessionFactory::resume_subagent(Some(&parent), config).await;
    if background {
        first.expect("后台执行须成功启动");
        let result = tokio::time::timeout(Duration::from_secs(10), result_rx)
            .await
            .expect("后台失败必须完成")
            .unwrap();
        assert!(!result.success);
    } else {
        assert!(first.is_err(), "持久化失败必须返回错误");
    }
    tokio::time::timeout(Duration::from_secs(10), finished_rx)
        .await
        .expect("旧执行必须完成")
        .unwrap();
    assert!(owner.pending_receipt().is_some(), "未确认交接身份必须保留");
    assert!(owner.receipt().is_none());
    let error = SessionFactory::resume_subagent(Some(&parent), resume_config(store, child_id))
        .await
        .err()
        .expect("未确认交接必须阻止 resume")
        .to_string();
    assert!(error.contains("handoff is unconfirmed"), "{error}");
}

/// [回归测试] 同步交接真实写入失败时，factory 明确拒绝恢复。
#[tokio::test(start_paused = true)]
async fn test_sync_unconfirmed_handoff_blocks_factory_resume() {
    test_unconfirmed_handoff_blocks_factory_resume(false).await;
}

/// [回归测试] 后台交接真实写入失败时，factory 明确拒绝恢复。
#[tokio::test(start_paused = true)]
async fn test_background_unconfirmed_handoff_blocks_factory_resume() {
    test_unconfirmed_handoff_blocks_factory_resume(true).await;
}
