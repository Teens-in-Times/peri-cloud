#[test]
fn settings_update_preserves_root_siblings_and_schema() {
    let temp = tempfile::tempdir().unwrap();
    let scope = make_scope(temp.path(), "project");
    let global = r#"{"$schema":"https://example.test/schema","config":{"active_alias":"sonnet"},"mcpServers":{"root":{"command":"echo"}},"langfuse":{"trace_sampling":0.4},"extension":{"keep":true}}"#;
    let (source, system) = make_system(&scope, make_inputs(Some(global), None, None));
    let current = system.resolve(scope.clone()).unwrap();
    let mut updated = current.settings().clone();
    updated.config.active_alias = "haiku".into();
    let next = system.update(&scope, current.revision(), &updated).unwrap();
    let (_, _, content) = source.writes().pop().unwrap();
    let document: Value = serde_json::from_str(&content).unwrap();
    assert_eq!(document["$schema"], "https://example.test/schema");
    assert_eq!(document["mcpServers"]["root"]["command"], "echo");
    assert_eq!(document["langfuse"]["trace_sampling"], 0.4);
    assert_eq!(document["extension"]["keep"], true);
    assert_eq!(next.settings().config.active_alias, "haiku");
}

#[test]
fn workspace_settings_write_only_relative_diff_without_global_credentials() {
    let temp = tempfile::tempdir().unwrap();
    let scope = make_scope(temp.path(), "project");
    let global = r#"{"config":{"active_alias":"sonnet","providers":[{"id":"global","type":"openai","apiKey":"sk-global-secret"}],"language":"en"}}"#;
    let workspace =
        r#"{"$schema":"workspace-schema","config":{"active_alias":"haiku"},"extension":"keep"}"#;
    let (source, system) = make_system(&scope, make_inputs(Some(global), Some(workspace), None));
    let current = system.resolve(scope.clone()).unwrap();
    let mut updated = current.settings().clone();
    updated.config.active_alias = "opus".into();
    let next = system.update(&scope, current.revision(), &updated).unwrap();
    let (path, _, content) = source.writes().pop().unwrap();
    assert_eq!(path, scope.cwd.join(".peri/settings.json"));
    assert!(!content.contains("sk-global-secret"));
    let document: Value = serde_json::from_str(&content).unwrap();
    assert_eq!(document["$schema"], "workspace-schema");
    assert_eq!(document["extension"], "keep");
    assert_eq!(document["config"]["active_alias"], "opus");
    assert!(document["config"].get("providers").is_none());
    assert_eq!(next.settings().config.providers.len(), 1);
}

#[test]
fn workspace_provider_edit_does_not_copy_global_api_keys() {
    let temp = tempfile::tempdir().unwrap();
    let scope = make_scope(temp.path(), "project");
    let global = r#"{"config":{"active_alias":"sonnet","providers":[{"id":"global","type":"openai","apiKey":"sk-global-secret"}]}}"#;
    let workspace = r#"{"config":{"active_alias":"haiku"}}"#;
    let (source, system) = make_system(&scope, make_inputs(Some(global), Some(workspace), None));
    let current = system.resolve(scope.clone()).unwrap();
    let mut updated = current.settings().clone();
    // setup wizard：编辑 global provider 的非凭据字段，并新增一个带自有密钥的 provider。
    updated.config.providers[0].base_url = "https://proxy.example.test/v1".into();
    updated.config.providers.push(
        serde_json::from_value(json!({
            "id": "workspace",
            "type": "anthropic",
            "apiKey": "sk-workspace-owned"
        }))
        .unwrap(),
    );
    let next = system.update(&scope, current.revision(), &updated).unwrap();
    let (_, _, content) = source.writes().pop().unwrap();
    assert!(
        !content.contains("sk-global-secret"),
        "workspace 写回不得复制全局凭据: {content}"
    );
    assert!(content.contains("sk-workspace-owned"));
    let document: Value = serde_json::from_str(&content).unwrap();
    assert_eq!(document["config"]["active_alias"], "haiku");
    let providers = document["config"]["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 2);
    let inherited = providers
        .iter()
        .find(|provider| provider["id"] == "global")
        .unwrap();
    assert_eq!(inherited["apiKey"], "");
    assert_eq!(inherited["baseUrl"], "https://proxy.example.test/v1");
    // endpoint 已改写，凭据绑定上下文不再一致：保存后不得回填全局密钥，
    // 也不得把它发往新 endpoint。
    assert!(
        next.settings().config.providers[0].api_key.is_empty(),
        "endpoint 改写后不得回填全局密钥"
    );
    let reloaded = system.resolve(scope.clone()).unwrap();
    assert!(
        reloaded.settings().config.providers[0].api_key.is_empty(),
        "重新 resolve 不得回填全局密钥"
    );
}

#[test]
fn mcp_update_preserves_top_level_siblings() {
    let temp = tempfile::tempdir().unwrap();
    let scope = make_scope(temp.path(), "project");
    let project = r#"{"$schema":"mcp-schema","mcpServers":{"old":{"command":"old"}},"extension":{"keep":true},"owner":"team"}"#;
    let (source, system) = make_system(&scope, make_inputs(None, None, Some(project)));
    let current = system.resolve(scope.clone()).unwrap();
    let config: McpConfigFile = serde_json::from_value(json!({
        "mcpServers": {"new": {"command": "new"}},
        "mcpCache": false
    }))
    .unwrap();
    system
        .update_mcp(&scope, current.revision(), &config)
        .unwrap();
    let document: Value = serde_json::from_str(&source.writes().pop().unwrap().2).unwrap();
    assert_eq!(document["$schema"], "mcp-schema");
    assert_eq!(document["extension"]["keep"], true);
    assert_eq!(document["owner"], "team");
    assert_eq!(document["mcpServers"]["new"]["command"], "new");
    assert!(document["mcpServers"].get("old").is_none());
    assert_eq!(document["mcpCache"], false);
}

#[test]
fn global_update_invalidates_other_scopes_sharing_global_settings() {
    let temp = tempfile::tempdir().unwrap();
    let first_scope = make_scope(temp.path(), "project-a");
    let second_scope = make_scope(temp.path(), "project-b");
    let source = Arc::new(FakeSource::default());
    for scope in [&first_scope, &second_scope] {
        source.set_inputs(scope, make_inputs(None, None, None));
    }
    let system = ConfigurationSystem::new(source);
    let first = system.resolve(first_scope.clone()).unwrap();
    system.resolve(second_scope.clone()).unwrap();
    let mut settings = first.settings().clone();
    settings.config.language = Some("en".into());
    let next = system
        .update(&first_scope, first.revision(), &settings)
        .unwrap();
    assert!(system.current(&second_scope).is_none());
    assert!(Arc::ptr_eq(&next, &system.current(&first_scope).unwrap()));
    assert_ne!(first.revision(), next.revision());
}

#[test]
fn settings_update_preserves_nested_mcp_domain_when_request_only_has_provider() {
    let temp = tempfile::tempdir().unwrap();
    let scope = make_scope(temp.path(), "project");
    let global = json!({
        "config": {
            "mcpServers": {"nested": {"command": "original"}},
            "mcpCache": false
        }
    });
    let (source, system) = make_system(&scope, make_inputs(Some(&global.to_string()), None, None));
    let current = system.resolve(scope.clone()).unwrap();
    let request: PeriConfig = serde_json::from_value(json!({
        "config": {"providers": [{"id": "new", "type": "openai", "apiKey": "new-key"}]}
    }))
    .unwrap();
    let next = system.update(&scope, current.revision(), &request).unwrap();
    let writes = source.writes();
    assert_eq!(writes.len(), 1);
    let document: Value = serde_json::from_str(&writes[0].2).unwrap();
    assert_eq!(
        document["config"]["mcpServers"],
        global["config"]["mcpServers"]
    );
    assert_eq!(document["config"]["mcpCache"], false);
    assert_eq!(
        next.mcp().mcp_servers["nested"].command.as_deref(),
        Some("original")
    );
    assert_eq!(next.mcp().mcp_cache, Some(false));
    assert_eq!(next.settings().config.providers[0].id, "new");
}

#[test]
fn workspace_settings_update_preserves_local_mcp_fields_and_schema() {
    let temp = tempfile::tempdir().unwrap();
    let scope = make_scope(temp.path(), "project");
    let global = r#"{"$schema":"global-schema","config":{}}"#;
    let workspace = json!({
        "$schema": "local-schema",
        "config": {
            "mcpServers": {"local": {"command": "local-command"}},
            "mcpCache": false
        },
        "extension": "keep"
    });
    let (source, system) = make_system(
        &scope,
        make_inputs(Some(global), Some(&workspace.to_string()), None),
    );
    let current = system.resolve(scope.clone()).unwrap();
    let request: PeriConfig = serde_json::from_value(json!({
        "$schema": "global-schema",
        "config": {"providers": [{"id": "new", "type": "openai", "apiKey": "new-key"}]}
    }))
    .unwrap();
    system.update(&scope, current.revision(), &request).unwrap();
    let writes = source.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].0, scope.cwd.join(".peri/settings.json"));
    let document: Value = serde_json::from_str(&writes[0].2).unwrap();
    assert_eq!(document["$schema"], "local-schema");
    assert_eq!(
        document["config"]["mcpServers"],
        workspace["config"]["mcpServers"]
    );
    assert_eq!(document["config"]["mcpCache"], false);
    assert_eq!(document["extension"], "keep");
}

#[test]
fn workspace_settings_update_cannot_import_global_mcp_fields_or_credentials() {
    let temp = tempfile::tempdir().unwrap();
    let scope = make_scope(temp.path(), "project");
    let global = r#"{"config":{"providers":[{"id":"global","type":"openai","apiKey":"global-provider-secret"}],"mcpServers":{"global":{"command":"global-command","env":{"TOKEN":"global-mcp-secret"}}},"mcpCache":false}}"#;
    let (source, system) = make_system(
        &scope,
        make_inputs(Some(global), Some(r#"{"config":{}}"#), None),
    );
    let current = system.resolve(scope.clone()).unwrap();
    let mut request = current.settings().clone();
    request.config.language = Some("fr".into());
    request.config.extra.insert("mcpCache".into(), json!(true));
    request.config.extra.insert(
        "mcpServers".into(),
        json!({
            "imported": {"command": "imported-command", "env": {"TOKEN": "global-mcp-secret"}}
        }),
    );
    let next = system.update(&scope, current.revision(), &request).unwrap();
    let writes = source.writes();
    assert_eq!(writes.len(), 1);
    let document: Value = serde_json::from_str(&writes[0].2).unwrap();
    assert!(document["config"].get("mcpServers").is_none());
    assert!(document["config"].get("mcpCache").is_none());
    assert!(document["config"].get("providers").is_none());
    assert!(!writes[0].2.contains("global-provider-secret"));
    assert!(!writes[0].2.contains("global-mcp-secret"));
    assert_eq!(next.mcp().mcp_cache, Some(false));
    assert!(next.mcp().mcp_servers.contains_key("global"));
    assert!(!next.mcp().mcp_servers.contains_key("imported"));
}

#[test]
fn mcp_update_rejects_foreign_server_sources_without_writing_or_publishing() {
    let temp = tempfile::tempdir().unwrap();
    let scope = make_scope(temp.path(), "project");
    let (source, system) = make_system(&scope, make_inputs(None, None, None));
    let current = system.resolve(scope.clone()).unwrap();
    let foreign_sources = [
        ConfigSource::Global(scope.global_settings.clone()),
        ConfigSource::Plugin,
        ConfigSource::Builtin {
            instance: "workspace".into(),
        },
        ConfigSource::Acp,
        ConfigSource::Project(temp.path().join("other-project/.mcp.json")),
    ];
    for foreign_source in foreign_sources {
        let mut server = make_default_mcp_server();
        server.command = Some("foreign-command".into());
        server.env = Some(HashMap::from([("TOKEN".into(), "foreign-secret".into())]));
        server.source = Some(foreign_source);
        let config = McpConfigFile {
            mcp_servers: HashMap::from([("foreign".into(), server)]),
            mcp_cache: None,
        };
        assert!(matches!(
            system.update_mcp(&scope, current.revision(), &config),
            Err(ConfigurationError::InvalidInput { .. })
        ));
        assert_eq!(source.io_counts().1, 0);
        assert!(source.writes().is_empty());
        assert!(Arc::ptr_eq(&current, &system.current(&scope).unwrap()));
    }
}

#[test]
fn mcp_update_accepts_unattributed_and_current_project_servers() {
    let temp = tempfile::tempdir().unwrap();
    let scope = make_scope(temp.path(), "project");
    let (source, system) = make_system(&scope, make_inputs(None, None, None));
    let current = system.resolve(scope.clone()).unwrap();
    let mut unattributed = make_default_mcp_server();
    unattributed.command = Some("unattributed-command".into());
    let mut local = make_default_mcp_server();
    local.command = Some("local-command".into());
    local.source = Some(ConfigSource::Project(scope.cwd.join(".mcp.json")));
    let config = McpConfigFile {
        mcp_servers: HashMap::from([("new".into(), unattributed), ("local".into(), local)]),
        mcp_cache: None,
    };
    let next = system
        .update_mcp(&scope, current.revision(), &config)
        .unwrap();
    let writes = source.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].0, scope.cwd.join(".mcp.json"));
    let document: Value = serde_json::from_str(&writes[0].2).unwrap();
    assert_eq!(
        document["mcpServers"]["new"]["command"],
        "unattributed-command"
    );
    assert_eq!(document["mcpServers"]["local"]["command"], "local-command");
    for server in next.mcp().mcp_servers.values() {
        assert_eq!(
            server.source,
            Some(ConfigSource::Project(scope.cwd.join(".mcp.json")))
        );
    }
}

#[test]
fn explanations_omit_environment_secrets_while_debug_preserves_inputs() {
    let temp = tempfile::tempdir().unwrap();
    let scope = make_scope(temp.path(), "project");
    let mut inputs = make_inputs(None, None, None);
    inputs
        .environment
        .insert("LANGFUSE_PUBLIC_KEY".into(), "pk-visible-secret".into());
    inputs
        .environment
        .insert("LANGFUSE_SECRET_KEY".into(), "sk-hidden-secret".into());
    let snapshot = ConfigurationSnapshot::resolve(scope, inputs.clone()).unwrap();
    let explanation = snapshot.explain(ConfigurationField::Observability);
    let explanation_rendered = format!("{explanation:?}");
    assert!(!explanation_rendered.contains("pk-visible-secret"));
    assert!(!explanation_rendered.contains("sk-hidden-secret"));
    let debug = format!("{snapshot:?} {inputs:?} {:?}", snapshot.observability());
    assert!(debug.contains("pk-visible-secret"));
    assert!(debug.contains("sk-hidden-secret"));
}

// ─── config.betas 投影与保存（Beta flag）────────────────────────────────────

/// 投影：workspace 覆盖 global，显式 false 关闭 global true；解释返回贡献者与规则。
#[test]
fn betas_projection_merges_layers_and_explains_sources() {
    use peri_acp_types::beta_flags::{BetaFlagOrigin, FULL_ASYNC_TOOLS};
    let temp = tempfile::tempdir().unwrap();
    let scope = make_scope(temp.path(), "project");
    let global = format!(r#"{{"config":{{"betas":{{"{FULL_ASYNC_TOOLS}":true}}}}}}"#);
    let workspace = format!(r#"{{"config":{{"betas":{{"{FULL_ASYNC_TOOLS}":false}}}}}}"#);
    let snapshot = ConfigurationSnapshot::resolve(
        scope.clone(),
        make_inputs(Some(&global), Some(&workspace), None),
    )
    .unwrap();
    assert!(
        !snapshot.flags().is_enabled(FULL_ASYNC_TOOLS),
        "workspace 显式 false 关闭 global true"
    );
    assert_eq!(
        snapshot
            .flags()
            .value(FULL_ASYNC_TOOLS)
            .map(|value| value.origin),
        Some(BetaFlagOrigin::Workspace)
    );

    let explanation = snapshot.explain(ConfigurationField::Betas);
    assert!(explanation
        .contributors
        .contains(&SourceIdentity::GlobalFile));
    assert!(explanation
        .contributors
        .contains(&SourceIdentity::WorkspaceFile));
    assert!(
        explanation.rule.contains("workspace wins"),
        "解释必须说明合并规则：{}",
        explanation.rule
    );

    // 未配置来源：一切按 false（快照缺失不意外开启能力）。
    let unset = ConfigurationSnapshot::resolve(
        make_scope(temp.path(), "other-project"),
        make_inputs(None, None, None),
    )
    .unwrap();
    assert!(unset.flags().is_empty());
    assert!(!unset.flags().is_enabled(FULL_ASYNC_TOOLS));
}

/// 未知 flag 键被忽略且不影响其他配置领域。
#[test]
fn betas_unknown_keys_are_ignored_without_touching_other_domains() {
    let temp = tempfile::tempdir().unwrap();
    let scope = make_scope(temp.path(), "project");
    let global = r#"{"config":{"language":"zh-CN","betas":{"not-a-flag":true}}}"#;
    let snapshot =
        ConfigurationSnapshot::resolve(scope, make_inputs(Some(global), None, None)).unwrap();
    assert!(snapshot.flags().is_empty(), "未知键不进入投影");
    assert_eq!(
        snapshot.settings().config.language.as_deref(),
        Some("zh-CN"),
        "未知键不影响同文件其他领域"
    );
    assert!(
        !snapshot
            .settings()
            .config
            .betas
            .overrides
            .contains_key("not-a-flag"),
        "未知键在解析后从内存配置剔除"
    );
}

/// 非 bool 值使该来源解析失败（不静默降级）。
#[test]
fn betas_non_bool_value_fails_that_source() {
    let temp = tempfile::tempdir().unwrap();
    let scope = make_scope(temp.path(), "project");
    let global = r#"{"config":{"betas":{"full-async-tools":1}}}"#;
    let error = ConfigurationSnapshot::resolve(scope, make_inputs(Some(global), None, None))
        .expect_err("非 bool 值必须解析失败");
    assert!(
        matches!(
            error,
            ConfigurationError::InvalidInput {
                source_identity: SourceIdentity::GlobalFile,
                domain: "settings",
                ..
            }
        ),
        "错误必须定位到来源与领域：{error:?}"
    );
}

/// 保存往返：workspace 只写相对 global 的差异键，并经 CAS 发布新快照。
#[test]
fn betas_save_writes_workspace_diff_and_publishes() {
    use peri_acp_types::beta_flags::FULL_ASYNC_TOOLS;
    let temp = tempfile::tempdir().unwrap();
    let scope = make_scope(temp.path(), "project");
    let (source, system) = make_system(
        &scope,
        make_inputs(Some(r#"{"config":{}}"#), Some(r#"{"config":{}}"#), None),
    );
    let current = system.resolve(scope.clone()).unwrap();
    let mut updated: PeriConfig = current.settings().clone();
    updated.config.betas.set(FULL_ASYNC_TOOLS, true);
    let next = system.update(&scope, current.revision(), &updated).unwrap();
    assert!(next.flags().is_enabled(FULL_ASYNC_TOOLS));

    let writes = source.writes();
    assert_eq!(writes.len(), 1);
    let (path, expected, content) = &writes[0];
    assert_eq!(path, &scope.cwd.join(".peri/settings.json"));
    assert_eq!(expected.as_deref(), Some(r#"{"config":{}}"#));
    let document: Value = serde_json::from_str(content).unwrap();
    assert_eq!(
        document["config"]["betas"][FULL_ASYNC_TOOLS],
        serde_json::json!(true)
    );
}

/// CAS 冲突不发布：revision 过期时拒绝写入并保留 current。
#[test]
fn betas_save_conflict_does_not_publish() {
    use peri_acp_types::beta_flags::FULL_ASYNC_TOOLS;
    let temp = tempfile::tempdir().unwrap();
    let scope = make_scope(temp.path(), "project");
    let (source, system) = make_system(&scope, make_inputs(None, None, None));
    let stale = system.resolve(scope.clone()).unwrap();
    source.mutate_inputs(&scope, |inputs| {
        inputs.global = Some(r#"{"config":{"language":"en"}}"#.into())
    });
    let current = system.resolve(scope.clone()).unwrap();
    let mut updated: PeriConfig = current.settings().clone();
    updated.config.betas.set(FULL_ASYNC_TOOLS, true);
    let error = system
        .update(&scope, stale.revision(), &updated)
        .unwrap_err();
    assert!(matches!(error, ConfigurationError::Conflict));
    assert!(source.writes().is_empty(), "冲突不写盘");
    assert!(Arc::ptr_eq(&current, &system.current(&scope).unwrap()));
    assert!(
        !system
            .current(&scope)
            .unwrap()
            .flags()
            .is_enabled(FULL_ASYNC_TOOLS),
        "冲突不发布新投影"
    );
}
