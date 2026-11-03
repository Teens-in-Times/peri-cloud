
/// 保留名反例（A3 / IF-D6 冻结约束 4）：外部 server 若用保留名接管，绝不能靠
/// 「按名字反查」静默继承 builtin 一等工具的判定（即移除 `mcp__*` 审批门）。
///
/// 该风险的落点是**加载期 typed error**（`McpConfigError::ReservedBuiltinInstanceName`，
/// 断言在配置侧 `mcp::builtin_apply`，owner I-02）。本测试锁定 permission 侧可证的
/// 两项前提：① 归一表命中的实例名全部是保留名（加载器有据可拦）；
/// ② 预留但未实现的实例名不进归一表 ⇒ 其 effective name 仍是未知 MCP 工具，保守敏感。
#[test]
fn builtin_reserved_name_is_not_parity_hijackable() {
    for instance in BUILTIN_MCP_INSTANCES {
        assert!(
            is_reserved_instance_name(instance.name),
            "归一表只允许以保留实例名为键（保留名由加载期 typed error 保护，A3）：{}",
            instance.name
        );
    }
    for reserved in ["web", "artifact", "cron", "workspace"] {
        assert!(
            is_reserved_instance_name(reserved),
            "{reserved} 应为保留实例名"
        );
    }

    // wave 3：`workspace` 已实现 ⇒ 其 effective name 进归一表，判定与原始名相等
    // （`Read` 不敏感）；**未声明**的工具名（下例 `NotATool`）不进归一表 ⇒ 仍是
    // 未知 MCP 工具、保守敏感——原「未声明 ⇒ 保守」的强度由后两条断言保留。
    assert_eq!(
        original_tool_name_of_effective("mcp__workspace__Read"),
        Some("Read")
    );
    assert!(!default_requires_approval("mcp__workspace__Read"));
    assert_eq!(
        original_tool_name_of_effective("mcp__workspace__NotATool"),
        None
    );
    assert!(default_requires_approval("mcp__workspace__NotATool"));

    // 「外部 server 名 `artifact` + 工具 `artifact`」的判定跟原始名走（另一结果
    // = 加载期 typed error，见配置侧断言）；两者都必须成立其一，不得是第三种。
    assert_eq!(
        default_requires_approval("mcp__artifact__artifact"),
        default_requires_approval("artifact")
    );
}

/// A19 / IF-G3：已迁移条目的 `name` 为 effective name；`mcp__` 前缀条目的
/// description 不再宣称「所有 `mcp__*` 一律敏感」（对 builtin 一等工具已不充分）。
#[test]
fn sensitive_entries_use_model_names_and_parity_description() {
    let entries = sensitive_tool_entries();
    let names: Vec<&str> = entries.iter().map(|entry| entry.name).collect();

    // system MCP 原名就是模型面名字。
    assert!(
        names.contains(&TOOL_WEBFETCH),
        "条目名应为原名 `{TOOL_WEBFETCH}`"
    );
    assert!(
        names.contains(&TOOL_WEBSEARCH),
        "条目名应为原名 `{TOOL_WEBSEARCH}`"
    );

    // 两个 Web 工具以 effective name 精确条目出现
    let web = peri_acp_types::builtin_mcp::find("web").expect("web 实例应有声明");
    for tool in web.tools {
        assert!(
            entries
                .iter()
                .any(|entry| entry.name == tool.effective_name && !entry.prefix_match),
            "`{}` 应以 effective name 的精确条目出现",
            tool.effective_name
        );
    }

    // artifact 不在敏感清单（IF-G2 决策 1）——条目表与判定同源
    let artifact = peri_acp_types::builtin_mcp::find("artifact").expect("artifact 实例应有声明");
    for tool in artifact.tools {
        assert!(
            !names.contains(&tool.effective_name),
            "`{}` 不应在敏感清单（artifact 不审批）",
            tool.effective_name
        );
    }

    // 前缀条目条文与归一后的判定一致
    let prefix_entry = entries
        .iter()
        .find(|entry| entry.prefix_match && entry.name == "mcp__")
        .expect("`mcp__` 前缀条目应存在");
    assert!(
        prefix_entry
            .description
            .contains("follow their original tool's rule"),
        "`mcp__` 条目条文必须说明 builtin 一等工具按原始名规则，实际：{}",
        prefix_entry.description
    );
}

/// N8：workspace 四个需审批条目以模型面 effective name 出现（裸名不得残留——
/// 迁移后模型面已不存在裸名），免审批的 Read/Glob/Grep 不得进清单，判定与
/// 条目同源。
#[test]
fn workspace_sensitive_entries_use_model_names() {
    let entries = sensitive_tool_entries();
    let names: Vec<&str> = entries.iter().map(|entry| entry.name).collect();

    let gated = [TOOL_BASH, TOOL_FOLDER_OPS, TOOL_WRITE, TOOL_EDIT];

    // 原名作为模型面条目名出现。
    for bare in gated {
        assert!(names.contains(&bare), "条目名应为原名 `{bare}`");
    }

    // 四个需审批工具以 effective name 的精确条目出现
    for original in gated {
        let effective = registry_effective_name("workspace", original);
        assert!(
            entries
                .iter()
                .any(|entry| entry.name == effective && !entry.prefix_match),
            "`{effective}` 应以 effective name 的精确条目出现"
        );
    }

    // 免审批的 workspace 工具不得进清单
    for original in ["Read", "Glob", "Grep"] {
        let effective = registry_effective_name("workspace", original);
        assert!(
            !names.contains(&effective),
            "`{effective}` 归一后走免审批分支，不得进敏感清单"
        );
    }

    // 判定与条目同源（两种名字形态逐项）
    for original in gated {
        let effective = registry_effective_name("workspace", original);
        assert!(
            default_requires_approval(effective),
            "`{effective}` 必须判定为需审批"
        );
        assert!(
            default_requires_approval(original),
            "裸名 `{original}` 必须判定为需审批"
        );
    }
    for original in ["Read", "Glob", "Grep"] {
        let effective = registry_effective_name("workspace", original);
        assert!(
            !default_requires_approval(effective),
            "`{effective}` 必须判定为免审批"
        );
    }

    // AcceptEdits 模式的编辑工具判定（Write / Edit / folder_operations 为真）
    assert!(is_edit_tool(registry_effective_name("workspace", "Write")));
    assert!(is_edit_tool(registry_effective_name("workspace", "Edit")));
    assert!(is_edit_tool(registry_effective_name(
        "workspace",
        "folder_operations"
    )));
    assert!(!is_edit_tool(registry_effective_name("workspace", "Bash")));
}

/// §8 第 17 行（F9）：模型可见 Markdown 的显示名从声明表解析 effective name。
///
/// 断言的是**渲染输出**：`cron_register` 迁移为 builtin 实例后已不存在裸名工具，
/// 渲染文本里出现裸名条目即点名一个不存在的工具。期望值从注册表
/// （`peri_acp_types::builtin_mcp`）派生而非复用生产查表助手——复用助手会让
/// 「助手退回裸名」这种回归与断言同向变化而无法失败。
#[test]
fn cron_sensitive_markdown_uses_registry_effective_name() {
    let rendered = format_sensitive_tools();
    let lines: Vec<&str> = rendered.lines().collect();
    assert_eq!(lines.len(), 12, "敏感工具完整列表应逐项渲染");

    let cron = peri_acp_types::builtin_mcp::find("cron").expect("cron 实例应有声明");
    let register = cron
        .tools
        .iter()
        .find(|tool| tool.original_name == "cron_register")
        .expect("cron 实例应声明 cron_register");
    let cron_line = format!(
        "- `{}` — scheduled task registration (can trigger arbitrary prompts later, equivalent to delegated execution rights)",
        register.effective_name
    );
    assert!(
        lines.contains(&cron_line.as_str()),
        "cron 条目显示名必须是注册表解析出的 effective name，实际渲染：\n{rendered}"
    );

    let monitor = peri_acp_types::builtin_mcp::find("monitor").expect("monitor 实例应有声明");
    let start = monitor
        .tools
        .iter()
        .find(|tool| tool.original_name == "monitor_start")
        .expect("monitor 实例应声明 monitor_start");
    assert_eq!(
        *lines.last().expect("敏感工具列表非空"),
        format!(
            "- `{}` — monitor command execution (starts a shell command and watches its output)",
            start.effective_name
        ),
        "末项显示名必须是注册表解析出的 effective name，实际渲染：\n{rendered}"
    );

    // 已失效裸名不得作为条目名出现；effective name 内部含 `cron_register` 子串，
    // 故按「行首条目名」判定而非整串包含。
    assert!(
        !lines
            .iter()
            .any(|line| line.starts_with("- `cron_register")),
        "渲染输出不得残留已失效的裸名条目：\n{rendered}"
    );

    // 未进敏感清单的 builtin 实例（artifact）不得因改名被误加进来
    for instance in ["artifact"] {
        let declared = peri_acp_types::builtin_mcp::find(instance).expect("实例应有声明");
        for tool in declared.tools {
            assert!(
                !lines
                    .iter()
                    .any(|line| line.starts_with(&format!("- `{}`", tool.effective_name))),
                "`{}` 不应在敏感清单（判定同源，A19）",
                tool.effective_name
            );
        }
    }
}

#[test]
fn deployment_closed_builtin_is_omitted_from_prompt_without_weakening_approval() {
    let disabled = std::collections::HashSet::from([
        "CronMiddleware".to_owned(),
        "MonitorMiddleware".to_owned(),
        "WorkspaceMiddleware".to_owned(),
    ]);
    let visible = format_sensitive_tools_for_disabled(&disabled);
    assert!(!visible.contains("mcp__cron__cron_register"));
    assert!(!visible.contains("mcp__monitor__monitor_start"));
    assert!(!visible.lines().any(|line| line.starts_with("- `Bash`")));
    assert!(visible.lines().any(|line| line.starts_with("- `WebFetch`")));
    assert!(default_requires_approval("cron_register"));
    assert!(default_requires_approval("monitor_start"));
    assert_eq!(format_sensitive_tools().lines().count(), 12);
}
