/// System MCP 的启动发现必须是本次 live round-trip。
///
/// 持久缓存命中只证明「过去某个版本列过这些工具」，不能作为本次启动的
/// transport/能力协商健康证据；普通 MCP 保持既有 cache 策略（版本命中级跳过网络）。
/// 两次真实启动使用独立的 pool / task owner，共用 workspace 身份与磁盘缓存根；
/// 第一轮完整关闭后再启动第二轮，普通 server 命中磁盘缓存，System server 仍须 live。
#[tokio::test]
async fn system_discovery_is_live_while_ordinary_server_uses_cache() {
    let fixture = tempfile::tempdir().unwrap();
    let cwd = fixture.path().join("project");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::write(cwd.join("versioned.js"), VERSIONED_DISCOVERY_SCRIPT).unwrap();
    let system_counter = fixture.path().join("system-tools-list.log");
    let ordinary_counter = fixture.path().join("ordinary-tools-list.log");
    let config: super::super::config::McpConfigFile = serde_json::from_value(serde_json::json!({
        "mcpServers": {
            "system-srv": {
                "command": "node",
                "args": ["versioned.js", system_counter.to_str().unwrap()],
                "system_mcp": true,
                "system_mcp_tools": [],
            },
            "ordinary-srv": {
                "command": "node",
                "args": ["versioned.js", ordinary_counter.to_str().unwrap()],
            },
        }
    }))
    .unwrap();
    let workspace = peri_acp_types::workspace::WorkspaceId::new();
    let cache_root = fixture.path().join("mcp-cache");
    for round in 1..=2 {
        // Reinitializing an already connected pool does not start new servers.
        // A fresh pool/cache object proves that round two reuses disk state,
        // while system discovery still obtains evidence from its new transport.
        let (mut tasks, spawner) = super::super::task_scope::McpTaskOwner::new();
        let mut pool = McpClientPool::new_pending_with_spawner(spawner);
        pool.resource_cache = crate::mcp::resource_cache::McpResourceCache::at(cache_root.clone());
        pool.bind_workspace_scope(workspace).unwrap();
        let pool = Arc::new(pool);
        let (status_tx, _status_rx) = tokio::sync::watch::channel(McpInitStatus::Pending);
        McpClientPool::initialize_config(
            pool.clone(),
            &cwd,
            config.clone(),
            Default::default(),
            status_tx,
            None,
        )
        .await;
        for name in ["system-srv", "ordinary-srv"] {
            let client = pool.client(name).unwrap_or_else(|| {
                panic!("第 {round} 轮 {name} 必须完成发现");
            });
            assert!(
                matches!(client.status, ClientStatus::Connected),
                "第 {round} 轮 {name} 必须完成发现，实际: {:?}",
                client.status
            );
            assert!(
                !client.tools.is_empty(),
                "第 {round} 轮 {name} 的发现结果必须进入 handle"
            );
            let evidence = pool
                .discovery_evidence(name)
                .unwrap_or_else(|| panic!("第 {round} 轮 {name} 必须提交本代发现证据"));
            assert!(
                evidence.is_complete(),
                "第 {round} 轮 {name} 的发现证据必须完整（含 live tools/list），实际: {evidence:?}"
            );
            assert_eq!(
                evidence.generation,
                pool.handle_generation(&client),
                "第 {round} 轮 {name} 的证据必须绑定当前句柄的代际"
            );
        }
        assert!(
            matches!(&*pool.init_status.read(), McpInitStatus::Ready { total: 2 }),
            "第 {round} 轮必须完成两个新连接的启动，实际: {:?}",
            *pool.init_status.read()
        );
        pool.begin_shutdown();
        tasks.begin_shutdown();
        assert_eq!(
            tasks.shutdown().await,
            super::super::task_scope::McpTaskShutdownReport::Complete
        );
        assert!(pool.shutdown().await.is_complete());
    }

    assert_eq!(
        discovery_lines(&system_counter),
        2,
        "System MCP 每轮都必须走 live tools/list：required=[] 也要 round-trip，\
         缓存命中的历史清单不能作为本次启动证据"
    );
    assert_eq!(
        discovery_lines(&ordinary_counter),
        1,
        "普通 MCP 第二轮必须命中持久缓存（保持既有 cache 策略，不被 System 严格化波及）"
    );
}

/// 重连必须换掉发现证据：旧代的成功证据不得被新代复用。
///
/// 场景：先成功发现（证据完整），随后 server 的 `tools/list` 变坏再重连。若证据
/// 按 server 名缓存而不换代，System 闸门会拿旧代证据放行一台已经报错的 server。
const RECONNECT_DISCOVERY_SCRIPT: &str = r#"
const fs = require('node:fs');
const marker = process.argv[2];
const readline = require('node:readline').createInterface({ input: process.stdin });
readline.on('line', line => {
  const request = JSON.parse(line);
  if (request.id === undefined) return;
  if (!['initialize', 'tools/list', 'resources/list', 'ping'].includes(request.method)) {
    process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: request.id,
      error: { code: -32601, message: 'Method not found' } }) + '\n');
    return;
  }
  // 标记文件出现后 tools/list 一律失败，模拟「重连时 server 工具面已损坏」。
  if (request.method === 'tools/list') {
    if (fs.existsSync(marker)) {
      process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: request.id,
        error: { code: -32603, message: 'tools unavailable' } }) + '\n');
    } else {
      process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: request.id,
        result: { tools: [] } }) + '\n');
    }
    return;
  }
  let result = {};
  if (request.method === 'initialize') result = {
    protocolVersion: '2025-11-25', capabilities: {},
    serverInfo: { name: 'reconnect-discovery', version: '1' },
  };
  if (request.method === 'resources/list') result = { resources: [] };
  process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: request.id, result }) + '\n');
});
"#;

#[tokio::test]
async fn reconnect_replaces_discovery_evidence_with_current_generation() {
    let fixture = tempfile::tempdir().unwrap();
    let cwd = fixture.path().join("project");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::write(cwd.join("reconnect.js"), RECONNECT_DISCOVERY_SCRIPT).unwrap();
    let marker = fixture.path().join("break-tools");
    let config = serde_json::from_value(serde_json::json!({
        "mcpServers": { "reconnect-srv": {
            "command": "node",
            "args": ["reconnect.js", marker.to_str().unwrap()],
            "system_mcp": true,
            "system_mcp_tools": [],
        } }
    }))
    .unwrap();
    let (mut tasks, spawner) = super::super::task_scope::McpTaskOwner::new();
    let pool = Arc::new(McpClientPool::new_pending_with_spawner(spawner));
    let (status_tx, _status_rx) = tokio::sync::watch::channel(McpInitStatus::Pending);
    McpClientPool::initialize_config(
        pool.clone(),
        &cwd,
        config,
        Default::default(),
        status_tx,
        None,
    )
    .await;

    let first = pool.client("reconnect-srv").expect("首轮必须完成发现");
    let first_evidence = pool
        .discovery_evidence("reconnect-srv")
        .expect("首轮成功必须提交本代证据");
    assert!(
        first_evidence.is_complete(),
        "首轮成功必须构成完整证据，实际: {first_evidence:?}"
    );

    std::fs::write(&marker, "").unwrap();
    let reconnect = pool.reconnect("reconnect-srv", None).await;
    assert!(
        reconnect.is_err(),
        "重连时 tools/list 失败必须返回错误，实际: {reconnect:?}"
    );
    let current = pool
        .client("reconnect-srv")
        .expect("失败的重连必须留下可核对状态，而不是空句柄");
    assert!(
        matches!(current.status, ClientStatus::Failed(_)),
        "重连的 tools/list 失败必须显式 Failed，实际: {:?}",
        current.status
    );
    let evidence = pool
        .discovery_evidence("reconnect-srv")
        .expect("重连尝试结束必须提交本代证据");
    assert!(
        evidence.initialize_ok && !evidence.tools_list_ok,
        "重连证据必须如实记录 initialize 成功 / tools/list 失败，实际: {evidence:?}"
    );
    assert_eq!(
        evidence.generation,
        pool.handle_generation(&current),
        "证据必须绑定重连后句柄的代际"
    );
    assert_ne!(
        evidence.generation,
        pool.handle_generation(&first),
        "重连必须换代，旧代证据不得复用"
    );

    pool.begin_shutdown();
    tasks.begin_shutdown();
    let _ = tasks.shutdown().await;
    assert!(pool.shutdown().await.is_complete());
}

/// 启动依赖优先推进：普通 MCP 的慢握手不得把 System 发现排在后面。
///
/// 普通 fixture 阻塞在 `initialize`（放行文件出现前不回复），System fixture 立即
/// 响应。断言 System 证据先完成——若实现回到 HashMap 顺序串行连接，这条会因
/// fixture 的确定性阻塞而失败，而不是靠 sleep 猜时序。
const GATED_ORDINARY_SCRIPT: &str = r#"
const fs = require('node:fs');
const release = process.argv[2];
const readline = require('node:readline').createInterface({ input: process.stdin });
readline.on('line', line => {
  const request = JSON.parse(line);
  if (request.id === undefined) return;
  if (!['initialize', 'tools/list', 'resources/list', 'ping'].includes(request.method)) {
    process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: request.id,
      error: { code: -32601, message: 'Method not found' } }) + '\n');
    return;
  }
  let result = {};
  if (request.method === 'initialize') {
    const until = Date.now() + 20000;
    while (!fs.existsSync(release) && Date.now() < until) {}
    result = { protocolVersion: '2025-11-25', capabilities: {},
      serverInfo: { name: 'gated-ordinary', version: '1' } };
  }
  if (request.method === 'tools/list') result = { tools: [] };
  if (request.method === 'resources/list') result = { resources: [] };
  process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: request.id, result }) + '\n');
});
"#;

#[tokio::test]
async fn system_requirement_is_discovered_before_gated_ordinary_server() {
    let fixture = tempfile::tempdir().unwrap();
    let cwd = fixture.path().join("project");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::write(cwd.join("gated.js"), GATED_ORDINARY_SCRIPT).unwrap();
    let release = fixture.path().join("release-ordinary");
    let config = serde_json::from_value(serde_json::json!({
        "mcpServers": {
            "ordered-system": {
                "command": "node",
                "args": ["empty.js"],
                "system_mcp": true,
                "system_mcp_tools": [],
            },
            "gated-ordinary": {
                "command": "node",
                "args": ["gated.js", release.to_str().unwrap()],
            },
        }
    }))
    .unwrap();
    std::fs::write(cwd.join("empty.js"), SCRIPT).unwrap();
    let (mut tasks, spawner) = super::super::task_scope::McpTaskOwner::new();
    let pool = Arc::new(McpClientPool::new_pending_with_spawner(spawner));
    let init_pool = pool.clone();
    let init_cwd = cwd.clone();
    let init = tokio::spawn(async move {
        let (status_tx, _status_rx) = tokio::sync::watch::channel(McpInitStatus::Pending);
        McpClientPool::initialize_config(
            init_pool,
            &init_cwd,
            config,
            Default::default(),
            status_tx,
            None,
        )
        .await;
    });

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !pool
        .discovery_evidence("ordered-system")
        .is_some_and(|evidence| evidence.is_complete())
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "System 发现不得被普通 server 的慢握手挡住"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // System 批次处理完即向启动等待方收口：普通 server 还卡在握手时，
    // systemPhase 必须已经是 settled，而整池 initPhase 仍是 initializing。
    while pool.snapshot()["systemPhase"] != "settled" {
        assert!(
            tokio::time::Instant::now() < deadline,
            "System 收口位必须在普通 server 放行前发布"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        pool.snapshot()["initPhase"],
        "initializing",
        "普通 MCP 未完成时整池不得发布 ready"
    );
    assert!(
        pool.client("gated-ordinary").is_none(),
        "普通 server 尚未放行，不应已经写入句柄（说明它排在 System 之前）"
    );

    std::fs::write(&release, "").unwrap();
    init.await.unwrap();
    let ordinary = pool.client("gated-ordinary").expect("放行后必须写入句柄");
    assert!(
        matches!(ordinary.status, ClientStatus::Connected),
        "放行后普通 server 必须完成连接，实际: {:?}",
        ordinary.status
    );

    pool.begin_shutdown();
    tasks.begin_shutdown();
    let _ = tasks.shutdown().await;
    assert!(pool.shutdown().await.is_complete());
}

#[path = "dynamic_initialize_test.rs"]
mod dynamic_initialization_tests;

#[path = "initialize_builtin_test.rs"]
mod builtin_initialization_tests;

#[tokio::test]
async fn session_only_initialization_does_not_read_host_config_or_builtins() {
    let pool = Arc::new(McpClientPool::new_pending());
    pool.set_builtin_available(false).unwrap();
    pool.set_stdio_available(false).unwrap();
    let (status_tx, status_rx) = tokio::sync::watch::channel(McpInitStatus::Pending);

    McpClientPool::run_initialize_session_only(pool.clone(), Path::new("."), status_tx).await;

    assert!(matches!(
        *status_rx.borrow(),
        McpInitStatus::Ready { total: 0 }
    ));
    assert!(pool.configs.read().is_empty());
}
