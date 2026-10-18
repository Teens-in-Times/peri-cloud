//! Native command registry shared by cloud entry points. No terminal runtime.
use async_trait::async_trait;
use peri_acp_types::command::command_route::{
    CommandEntryKind, CommandLifecycle, CommandProvenance, CommandSource, RouteEntry,
};
use peri_acp_types::command::{
    ArgKind, ArgSpec, ArgsSchema, CommandContext, CommandFeedback, CommandHandler, CommandOutcome,
    CommandRegistry, CommandResult, FeedbackChannel, FeedbackLevel, FlagSpec, PromptStopReason,
};
use serde::Serialize;
use std::sync::Arc;

/// Authenticated host context available to registered handlers through
/// `CommandContext::dep::<CloudCommandScope>()`. Contains no transport secrets.
#[derive(Clone)]
pub struct CloudCommandScope {
    pub principal_id: uuid::Uuid,
    pub session_id: uuid::Uuid,
    pub turn_id: uuid::Uuid,
    pub device_id: Option<uuid::Uuid>,
    pub(crate) mcp_store: Option<Arc<crate::mcp::McpServerStore>>,
}

/// Only trusted server assembly registers executable Rust handlers. HTTP exposes
/// discovery/execution, never handler installation or arbitrary shell evaluation.
pub fn native_registry() -> Arc<CommandRegistry> {
    let registry = Arc::new(CommandRegistry::new());
    for (name, aliases, description, schema) in [
        (
            "compact",
            vec!["compress"],
            "压缩上下文并保留原始历史",
            Some(ArgsSchema::default()),
        ),
        (
            "clear",
            vec!["cls", "reset"],
            "清空当前会话可见上下文，历史仍留存",
            Some(ArgsSchema::default()),
        ),
        (
            "rewind",
            vec!["undo"],
            "回退上下文到指定消息之前；当前须 --no-revert-files",
            Some(ArgsSchema {
                positionals: vec![ArgSpec {
                    name: "target_message_id".into(),
                    kind: ArgKind::String,
                    required: true,
                    description: None,
                }],
                flags: vec![FlagSpec {
                    name: "no-revert-files".into(),
                    short: None,
                    description: None,
                }],
                ..Default::default()
            }),
        ),
        (
            "loop",
            vec![],
            "将目标交给原 Agent 循环，沿用迭代和额度上限",
            None,
        ),
        (
            "mcp",
            vec![],
            "MCP 服务池（list / add / remove / enable / disable）",
            None,
        ),
    ] {
        registry
            .register(RouteEntry {
                fullname: format!("core:{name}"),
                aliases: aliases.into_iter().map(str::to_owned).collect(),
                description: description.into(),
                kind: CommandEntryKind::Command,
                category: Some("native".into()),
                args_schema: schema,
                handler: Arc::new(NativeCommand(name)),
                provenance: CommandProvenance {
                    source: CommandSource::Core,
                    lifecycle: CommandLifecycle::Connected,
                },
            })
            .expect("distinct native commands");
    }
    registry
}
/// Paths belong to the frozen executor, never the cloud host. Keep the native
/// tokenizer/required/choice/flag checks, but defer path existence to the handler
/// through authenticated remote tools (and their existing permission policy).
pub fn parse_arguments(
    schema: &ArgsSchema,
    args: &str,
) -> Result<peri_acp_types::command::ParsedArgs, String> {
    let mut remote_schema = schema.clone();
    for argument in remote_schema
        .positionals
        .iter_mut()
        .chain(remote_schema.named.iter_mut())
    {
        if argument.kind == ArgKind::Path {
            argument.kind = ArgKind::String;
        }
    }
    remote_schema.parse(args)
}

struct NativeCommand(&'static str);
#[async_trait]
impl CommandHandler for NativeCommand {
    async fn execute(&self, context: CommandContext) -> CommandOutcome {
        if self.0 == "loop" {
            CommandOutcome::Inject(format!(
                "<goal-message>Loop: {}</goal-message>",
                context.args
            ))
        } else if self.0 == "mcp" {
            mcp_command(context).await
        } else {
            CommandOutcome::Delegate(self.0.into())
        }
    }
}

fn command_done(context: CommandContext, level: FeedbackLevel, message: String) -> CommandOutcome {
    CommandOutcome::Done(CommandResult {
        messages: context.history,
        stop_reason: PromptStopReason::EndTurn,
        feedback: Some(CommandFeedback {
            level,
            message,
            channel: FeedbackChannel::UiOnly,
        }),
    })
}

async fn mcp_command(context: CommandContext) -> CommandOutcome {
    let store = context
        .dep::<CloudCommandScope>()
        .and_then(|scope| scope.mcp_store.clone());
    let Some(store) = store else {
        return command_done(
            context,
            FeedbackLevel::Error,
            "MCP 管理不可用：缺少账号上下文。".into(),
        );
    };
    let args = context.args.trim().to_owned();
    let mut parts = args.split_whitespace();
    let action = parts.next().unwrap_or("list");
    match action {
        "list" | "" => match store.list().await {
            Ok(servers) if servers.is_empty() => command_done(
                context,
                FeedbackLevel::Info,
                "尚未配置 MCP 服务。添加：/mcp add <名称> <https://...>".into(),
            ),
            Ok(servers) => {
                let mut message = String::from("MCP 服务：\n");
                for server in servers {
                    message.push_str(&format!(
                        "- {} {}（{}）\n",
                        server.name,
                        server.url,
                        if server.disabled { "已停用" } else { "已启用" }
                    ));
                }
                message.push_str(
                    "管理：/mcp add <名称> <URL>；/mcp remove <名称>；/mcp enable|disable <名称>。配置从下一轮对话生效。",
                );
                command_done(context, FeedbackLevel::Info, message)
            }
            Err(error) => command_done(context, FeedbackLevel::Error, error.to_string()),
        },
        "add" => {
            let (Some(name), Some(url)) = (parts.next(), parts.next()) else {
                return command_done(
                    context,
                    FeedbackLevel::Error,
                    "用法：/mcp add <名称> <https://...>".into(),
                );
            };
            if parts.next().is_some() {
                return command_done(
                    context,
                    FeedbackLevel::Error,
                    "URL 不能包含空白字符。".into(),
                );
            }
            match store.add(name, url).await {
                Ok(()) => command_done(
                    context,
                    FeedbackLevel::Info,
                    format!(
                        "已添加 MCP 服务 {name}。配置从下一轮对话生效；其工具名为 mcp__{name}__*，调用仍需审批。"
                    ),
                ),
                Err(error) => command_done(context, FeedbackLevel::Error, error.to_string()),
            }
        }
        "remove" | "rm" => {
            let Some(name) = parts.next() else {
                return command_done(
                    context,
                    FeedbackLevel::Error,
                    "用法：/mcp remove <名称>".into(),
                );
            };
            match store.remove(name).await {
                Ok(()) => command_done(
                    context,
                    FeedbackLevel::Info,
                    format!("已移除 MCP 服务 {name}。配置从下一轮对话生效。"),
                ),
                Err(error) => command_done(context, FeedbackLevel::Error, error.to_string()),
            }
        }
        "enable" | "disable" => {
            let Some(name) = parts.next() else {
                return command_done(
                    context,
                    FeedbackLevel::Error,
                    format!("用法：/mcp {action} <名称>"),
                );
            };
            match store.set_disabled(name, action == "disable").await {
                Ok(()) => command_done(
                    context,
                    FeedbackLevel::Info,
                    format!(
                        "已{} MCP 服务 {name}。配置从下一轮对话生效。",
                        if action == "disable" { "停用" } else { "启用" }
                    ),
                ),
                Err(error) => command_done(context, FeedbackLevel::Error, error.to_string()),
            }
        }
        "help" | "-h" | "--help" => command_done(
            context,
            FeedbackLevel::Info,
            "用法：\n/mcp list\n/mcp add <名称> <https://...>\n/mcp remove <名称>\n/mcp enable|disable <名称>\n只接受公网 HTTPS 端点；不保存任何凭据。".into(),
        ),
        _ => command_done(
            context,
            FeedbackLevel::Error,
            "用法：/mcp list | add <名称> <URL> | remove <名称> | enable|disable <名称>".into(),
        ),
    }
}

#[derive(Serialize)]
pub struct CommandDescriptor {
    pub name: String,
    pub aliases: Vec<String>,
    pub description: String,
    pub args: Option<ArgsSchema>,
    pub availability: &'static str,
    pub reason: Option<&'static str>,
}

pub fn catalog(registry: &CommandRegistry) -> Vec<CommandDescriptor> {
    let mut result: Vec<_> = registry
        .snapshot()
        .iter()
        .map(|entry| CommandDescriptor {
            name: entry.fullname.clone(),
            aliases: entry.aliases.clone(),
            description: entry.description.clone(),
            args: entry.args_schema.clone(),
            availability: if entry.fullname == "core:rewind" {
                "limited"
            } else {
                "available"
            },
            reason: (entry.fullname == "core:rewind").then_some(
                "跨设备尚无文件变更快照；仅 --no-revert-files 可用，不会在云主控本地恢复远程文件",
            ),
        })
        .collect();
    // These commands were terminal-owned panels, not native Agent handlers.
    // Expose every original name with truthful capability discovery.
    for (name, aliases, description, available, reason) in [
        ("model", vec![], "模型选择", true, None),
        ("login", vec![], "模型凭据设置", true, None),
        ("config", vec![], "当前配置和管理入口", true, None),
        (
            "threads",
            vec!["history", "resume", "his"],
            "历史会话与恢复",
            true,
            None,
        ),
        ("status", vec![], "当前任务状态", true, None),
        ("memory", vec![], "个人记忆", true, None),
        ("tasks", vec![], "任务状态和结果", true, None),
        ("setup", vec![], "账号关联和初始设置", true, None),
        (
            "agent",
            vec![],
            "子 Agent 定义",
            false,
            Some("云端尚未装配子 Agent 管理服务"),
        ),
        (
            "hooks",
            vec![],
            "Hook 事件",
            false,
            Some("云端尚未装配 Hook 管理服务"),
        ),
        (
            "mcp",
            vec![],
            "MCP 服务池",
            false,
            Some("云端尚未装配动态 MCP 管理服务"),
        ),
        (
            "plugin",
            vec![],
            "插件管理",
            false,
            Some("云端尚未装配插件管理服务"),
        ),
        (
            "cron-list",
            vec![],
            "持久定时任务（/schedule 或 /定时）",
            true,
            None,
        ),
        (
            "betas",
            vec![],
            "特性开关",
            false,
            Some("云端没有对应特性开关存储"),
        ),
        (
            "workflows",
            vec![],
            "工作流运行",
            false,
            Some("云端尚未装配工作流调度器"),
        ),
        (
            "theme",
            vec![],
            "终端主题",
            false,
            Some("QQ 没有本地终端主题；此命令仅适用于 TUI"),
        ),
    ] {
        if result
            .iter()
            .any(|entry| entry.name == format!("ui:{name}"))
        {
            continue;
        }
        result.push(CommandDescriptor {
            name: format!("ui:{name}"),
            aliases: aliases.into_iter().map(str::to_owned).collect(),
            description: description.into(),
            args: None,
            availability: if available { "adapted" } else { "unavailable" },
            reason,
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::McpServerStore;
    use peri_acp_types::command::DependencyBag;
    #[test]
    fn remote_paths_are_not_validated_on_the_cloud_filesystem() {
        let schema = ArgsSchema {
            positionals: vec![ArgSpec {
                name: "path".into(),
                kind: ArgKind::Path,
                required: true,
                description: None,
            }],
            ..Default::default()
        };
        assert_eq!(
            parse_arguments(&schema, "/remote-only/does-not-exist-on-cloud")
                .unwrap()
                .positionals,
            vec!["/remote-only/does-not-exist-on-cloud"]
        );
        assert!(parse_arguments(&schema, "").is_err());
        let registry = native_registry();
        assert_eq!(
            registry.resolve("/compress").unwrap().entry.fullname,
            "core:compact"
        );
        assert_eq!(registry.resolve("/mcp").unwrap().entry.fullname, "core:mcp");
    }

    struct NoEvents;
    #[async_trait]
    impl peri_acp_types::event::EventSink for NoEvents {
        async fn push_event(&self, _: &str, _: &peri_acp_types::event::ExecutorEvent, _: u32) {}
        async fn push_done(&self, _: &str, _: &str, _: Option<&str>) {}
    }

    async fn mcp_fixture() -> (
        tempfile::TempDir,
        crate::state::CloudJournal,
        std::sync::Arc<McpServerStore>,
    ) {
        let root = tempfile::tempdir().unwrap();
        let journal = crate::state::CloudJournal::open(root.path()).await.unwrap();
        let principal = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO principals(id,login,display_name,password_hash) VALUES(?,?,?,'fixture-not-a-password-hash')",
        )
        .bind(principal.to_string())
        .bind(principal.to_string())
        .bind("Fixture")
        .execute(&journal.identity_pool())
        .await
        .unwrap();
        let store = std::sync::Arc::new(McpServerStore::new(journal.identity_pool(), principal));
        (root, journal, store)
    }

    async fn run_mcp(store: std::sync::Arc<McpServerStore>, args: &str) -> (FeedbackLevel, String) {
        let mut dependencies = DependencyBag::new();
        dependencies.insert(
            std::any::TypeId::of::<CloudCommandScope>(),
            std::sync::Arc::new(CloudCommandScope {
                principal_id: uuid::Uuid::new_v4(),
                session_id: uuid::Uuid::new_v4(),
                turn_id: uuid::Uuid::new_v4(),
                device_id: None,
                mcp_store: Some(store),
            }),
        );
        let mut context = CommandContext::new(
            "session".into(),
            Vec::new(),
            String::new(),
            std::sync::Arc::new(NoEvents),
            tokio_util::sync::CancellationToken::new(),
            dependencies,
        );
        context.args = args.into();
        match NativeCommand("mcp").execute(context).await {
            CommandOutcome::Done(result) => {
                let feedback = result.feedback.expect("feedback");
                (feedback.level, feedback.message)
            }
            _ => panic!("mcp command must complete with feedback"),
        }
    }

    #[tokio::test]
    async fn mcp_command_lists_and_validates_without_leaking_credentials() {
        let (_root, _journal, store) = mcp_fixture().await;
        let (level, message) = run_mcp(store.clone(), "list").await;
        assert_eq!(level, FeedbackLevel::Info);
        assert!(message.contains("尚未配置"));

        let (level, message) = run_mcp(store.clone(), "add docs http://93.184.216.34/mcp").await;
        assert_eq!(level, FeedbackLevel::Error);
        assert!(message.contains("https"));

        let (level, message) = run_mcp(store.clone(), "add docs https://93.184.216.34/mcp").await;
        assert_eq!(level, FeedbackLevel::Info);
        assert!(message.contains("mcp__docs__"));
        let (level, message) = run_mcp(store.clone(), "list").await;
        assert_eq!(level, FeedbackLevel::Info);
        assert!(message.contains("docs"));
        assert!(message.contains("已启用"));

        let (level, _) = run_mcp(store.clone(), "disable docs").await;
        assert_eq!(level, FeedbackLevel::Info);
        let (level, message) = run_mcp(store, "list").await;
        assert_eq!(level, FeedbackLevel::Info);
        assert!(message.contains("已停用"));
    }
}
