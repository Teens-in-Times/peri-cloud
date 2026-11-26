/// C-INJ-01 focused 回归：typed bridge 的 direct 提升不改变 bridge 身份，
/// 且既有 public `build_tool_bridges` 的 deferred 默认行为不变。
#[cfg(test)]
mod direct_flag_tests {
    use super::*;
    use crate::mcp::client::ClientStatus;

    fn make_tool(tool_name: &str) -> Tool {
        serde_json::from_value(serde_json::json!({
            "name": tool_name,
            "description": "Read a file",
            "inputSchema": {
                "type": "object",
                "properties": { "path": { "type": "string" } }
            }
        }))
        .unwrap()
    }

    fn make_handle(server: &str, tools: Vec<Tool>, status: ClientStatus) -> Arc<McpClientHandle> {
        Arc::new(McpClientHandle {
            call_gate: Default::default(),
            name: server.to_string(),
            version: None,
            connected_at: None,
            protocol_version: None,
            cache_version: None,
            peer: None,
            tools,
            resources: vec![],
            status,
            oauth_status: Default::default(),
            source: None,
            url: None,
            skills_capable: false,
        })
    }

    fn make_builtin_handle(instance: &str) -> Arc<McpClientHandle> {
        Arc::new(McpClientHandle {
            call_gate: Default::default(),
            name: instance.to_string(),
            version: None,
            connected_at: None,
            protocol_version: None,
            cache_version: None,
            peer: None,
            tools: vec![],
            resources: vec![],
            status: ClientStatus::Connected,
            oauth_status: Default::default(),
            source: Some(crate::mcp::config::ConfigSource::Builtin {
                instance: instance.to_string(),
            }),
            url: None,
            skills_capable: false,
        })
    }

    /// host-only 发现过滤：bridge 的 `visible_to_model()` 等于注册表逐工具
    /// `model_visible`（builtin 来源）；非 builtin 来源不受声明表影响。
    ///
    /// 派生式断言：首个 host-only 条目（workflow 的 `WorkflowAgentResult`）随实例
    /// 注册落地后自动被覆盖，不需要改本用例。
    #[test]
    fn builtin_declared_model_visibility_is_applied_to_bridges() {
        for instance in peri_acp_types::builtin_mcp::BUILTIN_MCP_INSTANCES.iter() {
            for tool in instance.tools {
                let bridge = McpToolBridge::new(
                    instance.name,
                    &make_tool(tool.original_name),
                    make_builtin_handle(instance.name),
                );
                assert_eq!(
                    bridge.visible_to_model(),
                    tool.model_visible,
                    "{}/{} 的桥接可见性必须等于注册表 model_visible",
                    instance.name,
                    tool.original_name
                );
            }
        }
        // 非 builtin 来源（source: None）：同名工具名不受 builtin 声明表影响。
        let external = McpToolBridge::new(
            "workflow",
            &make_tool("WorkflowAgentResult"),
            make_handle("workflow", vec![], ClientStatus::Connected),
        );
        assert!(
            external.visible_to_model(),
            "非 builtin 来源不得消费 builtin 声明表"
        );
    }

    #[test]
    fn test_system_direct_flag_preserves_bridge_identity() {
        let bridge = McpToolBridge::new(
            "workspace",
            &make_tool("Read"),
            make_handle("workspace", vec![], ClientStatus::Disconnected),
        );
        assert!(!bridge.is_direct(), "new 缺省必须为 deferred");
        let name = bridge.name().to_string();
        let parameters = bridge.parameters();
        assert!(bridge.visible_to_model());

        let promoted = bridge.with_system_direct();
        assert!(promoted.is_direct());
        assert_eq!(promoted.name(), "Read");
        assert_ne!(promoted.name(), name);
        assert_eq!(promoted.original_tool_name(), "Read");
        assert_eq!(promoted.mcp_server_name(), Some("workspace"));
        assert_eq!(promoted.parameters(), parameters);
        assert!(
            promoted.visible_to_model(),
            "direct 不等于绕过 model visibility"
        );
    }

    /// typed builder 的输入取注册表中**声明 deferred** 的 builtin 工具。
    ///
    /// 只有输入确实声明为 deferred，「不提升」才是有信息的断言：wave 3 起 `workspace`
    /// 的 7 个工具全部 `direct: true`（注册表第五项），继续拿它当输入会变成「提升」的
    /// 反面 —— 断言会红，而修法绝不是放宽断言。名字从注册表派生，不新增第二份名单。
    #[test]
    fn test_build_tool_bridges_keeps_deferred_default_and_matches_typed() {
        let (instance, tool) = peri_acp_types::builtin_mcp::BUILTIN_MCP_INSTANCES
            .iter()
            .find_map(|instance| {
                instance
                    .tools
                    .iter()
                    .find(|tool| !tool.direct)
                    .map(|tool| (instance, tool))
            })
            .expect("注册表必须至少保留一个 deferred 声明的 builtin 工具");
        let pool = Arc::new(McpClientPool::new_pending());
        let handle = make_handle(
            instance.name,
            vec![make_tool(tool.original_name)],
            ClientStatus::Connected,
        );
        pool.clients
            .write()
            .insert(instance.name.to_string(), Arc::clone(&handle));

        let typed = build_typed_tool_bridges(&pool);
        let boxed = build_tool_bridges(&pool);
        assert_eq!(typed.len(), 1);
        assert_eq!(boxed.len(), typed.len());
        assert_eq!(boxed[0].name(), typed[0].name());
        assert!(
            !typed[0].is_direct(),
            "注册表声明 deferred 的 builtin 工具（{}）在 typed builder 中必须保持 deferred",
            tool.legacy_name
        );
        assert!(
            !boxed[0].is_direct(),
            "public builder 必须保持 deferred 默认"
        );
        // 既有 generation / binding leases 传递行为不得因提取 typed builder 而丢失
        assert_eq!(typed[0].server_generation, pool.handle_generation(&handle));
        assert!(typed[0].binding_leases.is_some());
    }

    #[test]
    fn remote_workspace_direct_tools_follow_live_list_without_registry_names() {
        let pool = Arc::new(McpClientPool::new_pending());
        let mut handle = make_handle(
            "workspace",
            vec![make_tool("NewRemoteTool")],
            ClientStatus::Connected,
        );
        Arc::get_mut(&mut handle).unwrap().source =
            Some(super::super::config::ConfigSource::WorkspaceRemote);
        pool.clients.write().insert("workspace".into(), handle);
        let typed = build_typed_tool_bridges(&pool);
        assert_eq!(typed.len(), 1);
        assert_eq!(typed[0].name(), "NewRemoteTool");
        assert!(typed[0].is_direct());
        assert_eq!(typed[0].builtin_mcp_instance(), None);

        let empty = Arc::new(McpClientPool::new_pending());
        let mut handle = make_handle("workspace", vec![], ClientStatus::Connected);
        Arc::get_mut(&mut handle).unwrap().source =
            Some(super::super::config::ConfigSource::WorkspaceRemote);
        empty.clients.write().insert("workspace".into(), handle);
        assert!(build_typed_tool_bridges(&empty).is_empty());
    }
}
