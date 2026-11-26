/// 同 [`install_parent_host`]，并可选注入 Langfuse bridge（后台子链从父 host 读取）。
pub(crate) fn install_parent_host_with_bridge(
    store: &SessionFixture,
    parent: &std::sync::Arc<peri_agent::session::Session>,
    langfuse_bridge: Option<Arc<dyn peri_agent::agent::LangfuseBridgeLike>>,
) -> BackgroundCapture {
    let capture = BackgroundCapture::default();
    let mut host = parent
        .subagent_host()
        .as_deref()
        .cloned()
        .unwrap_or_default();
    host.session_resources = Some(store.facade());
    if host.task_manager.is_none() {
        host.task_manager = Some(Arc::new(peri_agent::agent::async_tasks::TaskManager::new()));
    }
    if host.on_bg_complete.is_none() {
        host.on_bg_complete = Some(capture.logger());
    }
    if langfuse_bridge.is_some() {
        host.langfuse_bridge = langfuse_bridge;
    }
    host.parent_thread_id = parent.store().thread_id.clone();
    parent.set_subagent_host(host);
    capture
}

/// 让目录成为 git 工作区（工作区发现需要真实仓库证据）。
pub(crate) fn git_init(directory: &std::path::Path) {
    for args in [
        vec!["init", "-q"],
        vec![
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-qm",
            "base",
        ],
    ] {
        let output = std::process::Command::new("git")
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", directory)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .arg("-C")
            .arg(directory)
            .args(&args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git fixture failed");
    }
}

/// 预置可恢复 thread：创建（title 决定工具集恢复路径）+ 写消息 + 置非 active。
async fn preset_resumable_thread(
    fixture: &SessionFixture,
    id: &str,
    title: &str,
    parent_thread_id: Option<&str>,
    msgs: Vec<BaseMessage>,
) {
    let id = id.to_string();
    let mut meta = peri_agent::thread::ThreadMeta::new_at("/tmp/work", peri_time::now_wall());
    meta.id = id.clone();
    meta.title = Some(title.to_string());
    meta.parent_thread_id = parent_thread_id.map(|s| s.to_string());
    meta.hidden = true;
    fixture.create_thread(meta).await.unwrap();
    if !msgs.is_empty() {
        fixture.append_messages(&id, &msgs).await.unwrap();
    }
    fixture.update_thread_status(&id, "done").await.unwrap();
}

// 本文件经 mod.rs 的 `#[path = "tool_test.rs"]` 挂载；此路径加载方式下，
// rustc 不会为聚合根派生 `tool_test/` 子目录，子模块需显式 `#[path]` 指向。
#[path = "tool_test/active_message_test.rs"]
mod active_message_test;
#[path = "tool_test/async_default_test.rs"]
mod async_default_test;

#[path = "tool_test/bg_register_cancel_test.rs"]
mod bg_register_cancel_test;
#[path = "tool_test/events_contract_test.rs"]
mod events_contract_test;
#[path = "tool_test/fork_test.rs"]
mod fork_test;
#[path = "tool_test/input_contract_test.rs"]
mod input_contract_test;
#[path = "tool_test/integration_modes_test.rs"]
mod integration_modes_test;
#[path = "tool_test/integration_v2_test.rs"]
mod integration_v2_test;
#[path = "tool_test/invoke_test.rs"]
mod invoke_test;
#[path = "tool_test/middleware_chain_test.rs"]
mod middleware_chain_test;
#[path = "tool_test/mock_model.rs"]
mod mock_model;
#[path = "tool_test/model_tier_test.rs"]
mod model_tier_test;
#[path = "tool_test/resume_failure_test.rs"]
mod resume_failure_test;
#[path = "tool_test/resume_identity_test.rs"]
mod resume_identity_test;
#[path = "tool_test/resume_integration_test.rs"]
mod resume_integration_test;
#[path = "tool_test/resume_test.rs"]
mod resume_test;
#[path = "tool_test/sections_parity_test.rs"]
mod sections_parity_test;
#[path = "tool_test/session_isolation_test.rs"]
mod session_isolation_test;
#[path = "tool_test/subagent_approval_scope_test.rs"]
mod subagent_approval_scope_test;

#[path = "tool_test/mcp_activation_owner_test.rs"]
mod mcp_activation_owner_test;

/// W5 关闭矩阵 E2E（生产链路）：agent 面关闭（`SubAgentMiddleware` 链槽关闭位 =
/// `SUB_AGENT_FACE_CLOSED_KEY`）⇒ 本地来源**不可发现、不可激活**，且**零磁盘兜底**
/// （定义文件仍在磁盘上也不得被读取）。
#[tokio::test]
async fn closed_agent_face_hides_and_blocks_activation_without_disk_fallback() {
    use peri_acp_types::workspace_resources::ResourceScope;

    let dir = tempfile::tempdir().unwrap();
    let agents_dir = dir.path().join(".claude").join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("matrix-agent.md"),
        "---\nname: matrix-agent\ndescription: Matrix agent\n---\n\nMatrix.\n",
    )
    .unwrap();

    // 默认（面开启）：资源面可见、可解析（定义文件存在即命中）。
    let open_fixture = crate::mcp::agent_face_fixture::AgentFaceFixture::connect(dir.path()).await;
    let open = open_fixture.registry.local_catalog(true);
    assert!(
        open.iter().any(|entry| entry.id == "matrix-agent"),
        "面开启时候选可见: {open:?}"
    );
    drop(open_fixture);

    // 关闭位（与 A24 关闭集同一份 `disabled_middlewares` 派生）：同一磁盘内容不可见。
    let closed_fixture =
        crate::mcp::agent_face_fixture::AgentFaceFixture::connect(dir.path()).await;
    let closed_registry =
        crate::mcp::McpAgentRegistry::new(std::sync::Arc::clone(&closed_fixture.pool))
            .with_local_face_closed(true);
    assert!(
        closed_registry.local_catalog(true).is_empty(),
        "关闭位下本地来源不可发现"
    );
    assert!(
        closed_registry.resolve_local("matrix-agent", true).is_err(),
        "关闭位下不可激活（零磁盘兜底）"
    );
    // 链槽关闭键是唯一事实源常量（不是散落字面量）。
    assert_eq!(
        crate::assembly::SUB_AGENT_FACE_CLOSED_KEY,
        "SubAgentMiddleware"
    );
    let _ = ResourceScope::Project;
}

#[path = "tool_test/resume_archive_test.rs"]
mod resume_archive_test;
