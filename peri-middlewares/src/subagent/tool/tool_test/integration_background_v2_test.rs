use super::*;

#[tokio::test]
async fn test_p0_2_background_defined_skill_preload_once_after_parent_cancel() {
    use peri_agent::agent::events::ExecutorEvent;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;

    let dir = tempdir().unwrap();
    let agents_dir = dir.path().join(".claude").join("agents");
    let skills_dir = dir.path().join(".claude").join("skills").join("p0-2-skill");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::create_dir_all(&skills_dir).unwrap();
    std::fs::write(
        agents_dir.join("p0-2-bg.md"),
        "---\nname: p0-2-bg\ndescription: P0-2 background agent\nskills:\n  - p0-2-skill\n---\n\nRun the task.\n",
    )
    .unwrap();
    // 磁盘文件是**反例**（W4b：子链预载只查 MCP registry，不读盘）。
    std::fs::write(
        skills_dir.join("SKILL.md"),
        "---\nname: p0-2-skill\ndescription: P0-2 skill\n---\n\nDISK CONTENT MUST NOT BE READ\n",
    )
    .unwrap();

    let llm_calls = Arc::new(AtomicUsize::new(0));
    let preload_count = Arc::new(std::sync::Mutex::new(0));
    let llm_calls_clone = Arc::clone(&llm_calls);
    let preload_count_clone = Arc::clone(&preload_count);
    #[derive(Clone)]
    struct BackgroundSkillLLM {
        calls: Arc<AtomicUsize>,
        preload_count: Arc<std::sync::Mutex<usize>>,
    }
    impl BackgroundSkillLLM {
        async fn respond(
            &self,
            request: peri_model::ModelRequest,
            cancellation: tokio_util::sync::CancellationToken,
        ) -> Vec<peri_model::ModelResult<peri_model::ModelStreamEvent>> {
            use crate::subagent::test_support::*;
            let _ = &cancellation;
            let messages = base_messages(&request);
            let defined = defined_tools(&request);
            let _tools: Vec<&dyn BaseTool> = defined.iter().map(|t| t as &dyn BaseTool).collect();

            self.calls.fetch_add(1, Ordering::SeqCst);
            assert!(
                messages
                    .iter()
                    .any(|message| message.content().contains("p0-2 background prompt")),
                "before_agent must observe the prompt only after Receive"
            );
            *self.preload_count.lock().unwrap() = messages
                .iter()
                .filter(|message| message.content().contains("P0-2 BACKGROUND SKILL MARKER"))
                .count();
            text_events("p0-2 background done")
        }
    }
    crate::subagent::test_support::fixture_model_impl!(BackgroundSkillLLM);

    let parent_cancel = AgentCancellationToken::new();
    let registry = Arc::new(peri_agent::agent::async_tasks::TaskManager::new());
    let (bg_tx, mut bg_rx) = mpsc::unbounded_channel::<ExecutorEvent>();
    let host = HostFixture::open_in_with_background(
        dir.path(),
        "fixture-iv2-skillpreload",
        Arc::clone(&registry),
        bg_tx.clone(),
    )
    .await;
    let tool = host.bind(
        SubAgentTool::new(
            Arc::new(vec![]),
            None,
            Arc::new(move |_: Option<&str>| {
                crate::subagent::test_support::fixture_source(
                    std::sync::Arc::new(BackgroundSkillLLM {
                        calls: Arc::clone(&llm_calls_clone),
                        preload_count: Arc::clone(&preload_count_clone),
                    }),
                    "fixture-scripted",
                )
            }),
            dir.path().to_str().unwrap().to_string(),
        )
        .with_cancel(parent_cancel.clone())
        .with_task_manager(registry)
        .with_bg_event_sender(bg_tx),
    );
    let tool = host.bind(with_agent_face(tool, dir.path()).await);
    let tool = super::with_skill_registry(
        tool,
        "workspace",
        &[("p0-2-skill", "P0-2 BACKGROUND SKILL MARKER\n")],
    );

    let started = tool
        .invoke(
            serde_json::json!({
                "subagent_type": "p0-2-bg",
                "run_in_background": true,
                "prompt": "p0-2 background prompt",
                "cwd": dir.path().to_str().unwrap(),
            }),
            host.context(&[]),
        )
        .await
        .expect("background defined subagent should start");
    assert!(started.contains("Background task"));
    parent_cancel.cancel();
    host.cancel_parent();

    let mut lifecycle = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(event) = bg_rx.recv().await {
            match event.payload().clone() {
                ExecutorEvent::SubagentStarted { is_background, .. } => {
                    assert!(is_background);
                    lifecycle.push("started");
                }
                ExecutorEvent::SubagentStopped { .. } => lifecycle.push("stopped"),
                ExecutorEvent::BackgroundTaskCompleted(result) => {
                    assert!(result.success);
                    assert!(result.output.contains("p0-2 background done"));
                    lifecycle.push("completed");
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("background defined subagent must complete within the bounded receiver timeout");

    assert_eq!(lifecycle, ["started", "stopped", "completed"]);
    assert_eq!(llm_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        *preload_count.lock().unwrap(),
        1,
        "explicit skill preload must occur exactly once on execute_bg.rs"
    );
}
