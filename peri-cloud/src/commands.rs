//! Native command registry shared by cloud entry points. No terminal runtime.
use async_trait::async_trait;
use peri_acp_types::command::command_route::{
    CommandEntryKind, CommandLifecycle, CommandProvenance, CommandSource, RouteEntry,
};
use peri_acp_types::command::{
    ArgKind, ArgSpec, ArgsSchema, CommandContext, CommandHandler, CommandOutcome, CommandRegistry,
    FlagSpec,
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
    pub device_id: uuid::Uuid,
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
        } else {
            CommandOutcome::Delegate(self.0.into())
        }
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
            "定时任务",
            false,
            Some("云端尚未装配定时任务调度器"),
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
    }
}
