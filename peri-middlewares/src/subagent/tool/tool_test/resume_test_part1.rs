
/// [回归测试] fork resume 继承父工具，并因耗尽新 run 的语义预算而失败。
///
/// 历史背景：旧夹具固定调用 ID 并持续请求不存在工具，基线提前报执行错误而非耗尽预算。
/// 每轮改为有效工具及唯一调用身份，并核对 typed 失败原因，避免只按模型调用数猜测预算。
#[tokio::test]
async fn test_resume_thread_id_fork_title_uses_parent_tools_and_default_iterations() {
    let dir = tempdir().unwrap();
    let store = SessionFixture::open_in(dir.path()).await;
    let cwd = store.workspace_cwd();
    let parent_id = store
        .create_thread(ThreadMeta::new_at(cwd.clone(), peri_time::now_wall()))
        .await
        .expect("建立父会话失败");
    // 父会话句柄：resume 路径经它校验「owning parent session」
    let parent = peri_agent::session::Session::new(
        std::sync::Arc::from(cwd.as_str()),
        peri_agent::session::FrozenContext::builder().build(),
        Some(parent_id.clone()),
    );
    install_parent_host(&store, &parent);
    let id = uuid::Uuid::now_v7().to_string();
    preset_resumable_thread(
        &store,
        &id,
        "fork",
        Some(parent_id.as_str()),
        vec![BaseMessage::human("task")],
    )
    .await;

    // 计数 + 工具捕获 LLM：每轮调用继承的工具 → 循环持续到迭代上限
    let llm_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let tools_capture: Arc<std::sync::Mutex<Vec<String>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let calls_clone = Arc::clone(&llm_calls);
    let tools_clone = Arc::clone(&tools_capture);
    #[derive(Clone)]
    struct ForkLoopLLM {
        calls: Arc<std::sync::atomic::AtomicUsize>,
        captured: Arc<std::sync::Mutex<Vec<String>>>,
    }
    impl ForkLoopLLM {
        async fn respond(
            &self,
            request: peri_model::ModelRequest,
            cancellation: tokio_util::sync::CancellationToken,
        ) -> Vec<peri_model::ModelResult<peri_model::ModelStreamEvent>> {
            use crate::subagent::test_support::*;
            let _ = &cancellation;
            let _messages = base_messages(&request);
            let defined = defined_tools(&request);
            let tools: Vec<&dyn BaseTool> = defined.iter().map(|t| t as &dyn BaseTool).collect();

            let call_index = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            *self.captured.lock().unwrap() = tools.iter().map(|t| t.name().to_string()).collect();
            tool_events_from_react(vec![peri_agent::agent::react::ToolCall::new(
                format!("fork-resume-{call_index}"),
                "Read",
                serde_json::json!({}),
            )])
        }
    }
    crate::subagent::test_support::fixture_model_impl!(ForkLoopLLM);

    let parent_tools = vec![make_tool("Read"), make_tool("Agent")];
    let t = SubAgentTool::new(
        Arc::new(parent_tools),
        None,
        Arc::new(move |_: Option<&str>| {
            crate::subagent::test_support::fixture_source(
                std::sync::Arc::new(ForkLoopLLM {
                    calls: Arc::clone(&calls_clone),
                    captured: Arc::clone(&tools_clone),
                }),
                "fixture-scripted",
            )
        }),
        "/tmp".to_string(),
    )
    .with_session_resources(store.facade())
    .with_parent_thread_id(parent_id.clone())
    .with_parent_session(parent.clone());

    let result = t
        .invoke(
            serde_json::json!({
                "resume_thread_id": id.clone(),
            }),
            preset_child_ctx(&id, "."),
        )
        .await;
    // 迭代上限耗尽 → MaxIterationsExceeded 错误（fork resume 上限 = DEFAULT_SUBAGENT_MAX_ITERATIONS）
    let error = result.unwrap_err();
    let failure = error
        .downcast_ref::<peri_agent::session::subagent::SubagentFailure>()
        .expect("fork resume must fail at the execution budget, not preflight or tool dispatch");
    assert_eq!(failure.child_thread_id(), id);
    let peri_agent::error::AgentError::MaxIterationsExceeded(budget) = failure.error() else {
        panic!("expected iteration budget exhaustion, got: {failure:?}");
    };
    let err = error.to_string();
    assert!(
        err.contains("child_thread_id") && err.contains("execution failed"),
        "错误文本应带 child_thread_id 前缀（可恢复）: {}",
        err
    );
    assert_eq!(
        llm_calls.load(std::sync::atomic::Ordering::SeqCst),
        *budget,
        "fork resume 必须耗尽配置预算，不能因无效工具调用提前退出"
    );
    assert_eq!(
        *budget,
        crate::subagent::DEFAULT_SUBAGENT_MAX_ITERATIONS,
        "fork resume 的当前默认预算必须与 fork 一致"
    );
    let captured = tools_capture.lock().unwrap();
    assert!(
        captured.contains(&"Agent".to_string()),
        "fork resume 应继承父工具集（无过滤，含 Agent）: {:?}",
        *captured
    );
}

/// agent-def resume：title == agent_id → load_agent_def 重新应用过滤
/// （tools 白名单 + Agent 恒排除；与 fork resume 的"父工具集无过滤"区分；
/// build_result 的 skill_names / system_prompt 被 resume_config_base 丢弃——
/// R-H1 / F4，不重复注入）
#[tokio::test]
async fn test_resume_thread_id_agent_def_refilters_tools() {
    let dir = tempdir().unwrap();
    let agents_dir = dir.path().join(".claude").join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("resume-agent.md"),
        "---\nname: resume-agent\ndescription: Resume filter test\ntools:\n  - Read\n---\n\nYou are resumable.\n",
    )
    .unwrap();

    let store = SessionFixture::open_in(dir.path()).await;
    let cwd = store.workspace_cwd();
    let parent_id = store
        .create_thread(ThreadMeta::new_at(cwd.clone(), peri_time::now_wall()))
        .await
        .expect("建立父会话失败");
    // 父会话句柄：resume 路径经它校验「owning parent session」
    let parent = peri_agent::session::Session::new(
        std::sync::Arc::from(cwd.as_str()),
        peri_agent::session::FrozenContext::builder().build(),
        Some(parent_id.clone()),
    );
    install_parent_host(&store, &parent);
    let id = uuid::Uuid::now_v7().to_string();
    preset_resumable_thread(
        &store,
        &id,
        "resume-agent",
        Some(parent_id.as_str()),
        Vec::new(),
    )
    .await;

    let tools_capture: Arc<std::sync::Mutex<Vec<String>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let tools_capture_clone = Arc::clone(&tools_capture);
    #[derive(Clone)]
    struct ResumeFilterLLM {
        captured: Arc<std::sync::Mutex<Vec<String>>>,
    }
    impl ResumeFilterLLM {
        async fn respond(
            &self,
            request: peri_model::ModelRequest,
            cancellation: tokio_util::sync::CancellationToken,
        ) -> Vec<peri_model::ModelResult<peri_model::ModelStreamEvent>> {
            use crate::subagent::test_support::*;
            let _ = &cancellation;
            let _messages = base_messages(&request);
            let defined = defined_tools(&request);
            let tools: Vec<&dyn BaseTool> = defined.iter().map(|t| t as &dyn BaseTool).collect();

            *self.captured.lock().unwrap() = tools.iter().map(|t| t.name().to_string()).collect();
            text_events("resume-filter-done")
        }
    }
    crate::subagent::test_support::fixture_model_impl!(ResumeFilterLLM);

    let parent_tools = vec![make_tool("Read"), make_tool("Write"), make_tool("Agent")];
    let t = SubAgentTool::new(
        Arc::new(parent_tools),
        None,
        Arc::new(move |_: Option<&str>| {
            crate::subagent::test_support::fixture_source(
                std::sync::Arc::new(ResumeFilterLLM {
                    captured: Arc::clone(&tools_capture_clone),
                }),
                "fixture-scripted",
            )
        }),
        "/tmp".to_string(),
    )
    .with_session_resources(store.facade())
    .with_parent_thread_id(parent_id.clone())
    .with_parent_session(parent.clone());
    let t = with_agent_face(t, dir.path()).await;

    let result = t
        .invoke(
            serde_json::json!({
                "resume_thread_id": id.clone(),
                "cwd": cwd.clone(),
            }),
            preset_child_ctx(&id, "."),
        )
        .await
        .expect("agent-def resume 应成功");
    assert!(
        result.contains(&format!("child_thread_id: {}", id)),
        "完成文本应带 child_thread_id: {}",
        result
    );
    assert!(
        result.contains("resume-filter-done"),
        "agent-def resume 应执行完成: {}",
        result
    );
    let captured = tools_capture.lock().unwrap();
    assert_eq!(
        captured.as_slice(),
        &["Read"],
        "agent-def resume 必须按 tools 白名单重新过滤（含 Agent 排除）: {:?}",
        *captured
    );
}

/// UUID 两侧空格和多余分支字段不能改变恢复优先级；非字符串 prompt 视为缺省。
#[tokio::test]
async fn test_resume_trimmed_id_wins_over_mcp_fork_and_invalid_model() {
    let dir = tempdir().unwrap();
    write_test_agent(&dir);
    let store = SessionFixture::open_in(dir.path()).await;
    let cwd = store.workspace_cwd();
    let parent_id = store
        .create_thread(ThreadMeta::new_at(cwd.clone(), peri_time::now_wall()))
        .await
        .expect("建立父会话失败");
    // 父会话句柄：resume 路径经它校验「owning parent session」
    let parent = peri_agent::session::Session::new(
        std::sync::Arc::from(cwd.as_str()),
        peri_agent::session::FrozenContext::builder().build(),
        Some(parent_id.clone()),
    );
    install_parent_host(&store, &parent);
    let id = uuid::Uuid::now_v7().to_string();
    preset_resumable_thread(&store, &id, "test-agent", Some(parent_id.as_str()), vec![]).await;
    let tool = with_agent_face(make_subagent_tool(vec![]), dir.path())
        .await
        .with_session_resources(store.facade())
        .with_parent_thread_id(parent_id.clone())
        .with_parent_session(parent.clone());
    let result = tool
        .invoke(
            serde_json::json!({
                "resume_thread_id": format!("  {id}\n"),
                "subagent_type": "mcp__missing__agent",
                "fork": true,
                "model": "invalid-model",
                "prompt": null,
                "cwd": cwd.clone()
            }),
            preset_child_ctx(&id, "."),
        )
        .await
        .unwrap();
    assert!(result.starts_with(&format!("child_thread_id: {id}\n")));
    assert!(result.contains("Continue your previous task where you left off."));
    // 恢复线程为 hidden；按真实根 ID 查询包含隐藏线程的会话树。
    let session_threads = store.list_session_threads(&id).await.unwrap();
    assert_eq!(session_threads.len(), 1, "恢复不得 fork 子线程");
    assert_eq!(session_threads[0].id, id);
    assert_eq!(
        store.load_meta(&id).await.unwrap().agent_status,
        peri_agent::thread::AgentStatus::Done
    );
}
