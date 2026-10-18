//! Chat command vocabulary; adapters share the same deterministic controls.
use super::{ChannelRoute, Gateway, GatewayError, GatewayResult};
use crate::identity::DeviceRecord;
use uuid::Uuid;

const ALIASES: &[(&str, &str)] = &[
    ("/schedule", "/定时"),
    ("/cron-list", "/定时"),
    ("/commands", "/help"),
    ("/命令", "/help"),
    ("/新会话", "/new"),
    ("/压缩", "/compact"),
    ("/menu", "/菜单"),
    ("/status", "/状态"),
    ("/cancel", "/取消"),
    ("/stop", "/取消"),
    ("/停止", "/取消"),
    ("/result", "/结果"),
    ("/devices", "/设备"),
    ("/connect", "/连接"),
    ("/workspace", "/工作区"),
    ("/model", "/模型"),
    ("/permissions", "/权限"),
    ("/usage", "/用量"),
    ("/memory", "/记忆"),
    ("/settings", "/设置"),
    ("/gateway", "/网关"),
    ("/context", "/pwd"),
    ("/core:new", "/new"),
    ("/core:help", "/help"),
    ("/core:pwd", "/pwd"),
    ("/tasks", "/状态"),
    ("/config", "/设置"),
    ("/login", "/设置"),
    ("/setup", "/设置"),
    ("/history", "/threads"),
    ("/his", "/threads"),
];

pub(super) fn normalize(text: &str) -> String {
    let text = text.trim();
    let (verb, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    if !verb.starts_with('/') {
        return text.to_owned();
    }
    let lower = verb.to_ascii_lowercase();
    let lower = lower
        .strip_prefix("/ui:")
        .map_or_else(|| lower.clone(), |v| format!("/{v}"));
    let canonical = ALIASES
        .iter()
        .find(|(alias, _)| *alias == lower)
        .map_or(lower.as_str(), |(_, target)| *target);
    let rest = if canonical == "/权限" {
        match rest.trim() {
            "default" => "默认",
            "edit" => "编辑",
            "all" => "全部",
            other => other,
        }
    } else {
        rest.trim()
    };
    let rest = if canonical == "/模型" && rest == "default" {
        "默认"
    } else {
        rest
    };
    if rest.is_empty() && canonical == "/resume" {
        return "/threads".into();
    }
    if rest.is_empty() {
        canonical.to_owned()
    } else {
        format!("{canonical} {rest}")
    }
}

pub(super) fn help() -> &'static str {
    "QQ 命令（官方机器人与个人 QQ 通用）\n\n会话\n/new：当前设备和目录开启空白会话，保留权限与模型，旧历史保留\n/clear（/cls、/reset）：清空当前上下文，保留历史\n/rewind 消息ID --no-revert-files：回退上下文，不改文件\n/loop 目标：按原 Agent 循环执行目标\n/history [页码|messages]：会话列表或本会话消息 ID\n/resume 会话ID：恢复历史会话\n/compact：生成摘要并继续当前会话，保留原始历史；会调用模型并计入额度\n/status：任务状态\n/cancel 或 /stop：请求取消当前任务\n/result [页码]：查看已存结果\n/pwd 或 /context：查看当前会话、设备、目录与权限\n\n设备与配置\n/devices：选择设备\n/connect 设备ID [绝对路径]：连接设备\n/workspace [绝对路径]：选择或切换目录\n/model [模型名]：查看或切换模型，default 恢复默认\n/permissions [default|edit|all]：工具权限\n/schedule 或 /定时：主动聊天与定时任务，查看创建/暂停/恢复/删除用法\n/usage：用量\n/memory：个人记忆\n/settings：个人设置\n/gateway：管理员私聊管理白黑名单\n\n/menu：打开按钮菜单\n/help 或 /commands：本帮助\n中文命令继续可用；/新会话、/压缩也可使用。\n/new 和 /compact 须等待当前任务结束；有未确认的执行状态时不会强行清空或切换。"
}

impl Gateway {
    pub(super) fn command_help(&self, args: &str) -> String {
        if args.is_empty() {
            return format!(
                "{}\n\n完整目录和接入状态：/help all\n单条命令：/help core:rewind",
                help()
            );
        }
        let catalog = self.command_catalog();
        let words: Vec<_> = args.split_whitespace().collect();
        if words.first() == Some(&"all") {
            let page = words
                .get(1)
                .and_then(|p| p.parse::<usize>().ok())
                .unwrap_or(1)
                .max(1);
            let mut text = format!("完整命令目录 · 第 {page} 页\n");
            for command in catalog.iter().skip((page - 1).saturating_mul(6)).take(6) {
                text.push_str(&format!(
                    "\n/{} [{}]\n{}{}\n",
                    command.name,
                    command.availability,
                    command.description,
                    command.reason.map_or(String::new(), |r| format!("\n{r}"))
                ));
            }
            if page.saturating_mul(6) < catalog.len() {
                text.push_str(&format!("\n下一页：/help all {}", page + 1));
            }
            return text;
        }
        let name = args.trim_start_matches('/');
        catalog
            .into_iter()
            .find(|c| {
                c.name == name
                    || c.name.strip_prefix("core:") == Some(name)
                    || c.name.strip_prefix("ui:") == Some(name)
                    || c.aliases.iter().any(|a| a == name)
            })
            .map(|c| {
                format!(
                    "/{} [{}]\n{}{}\n别名：{}",
                    c.name,
                    c.availability,
                    c.description,
                    c.reason.map_or(String::new(), |r| format!("\n{r}")),
                    c.aliases.join("、")
                )
            })
            .unwrap_or_else(|| "没有注册该命令。/help all 查看目录。".into())
    }

    pub(super) async fn new_chat_session(
        &self,
        route: &ChannelRoute,
        principal: Uuid,
        device: &DeviceRecord,
        old: Uuid,
    ) -> GatewayResult<String> {
        let state = self.journal().session(principal, old).await?;
        if !state.execution_settled() || self.journal().has_unconfirmed_turn(old).await? {
            return Err(GatewayError::Busy);
        }
        let agent = self
            .connector
            .open_workspace(principal, device, &state.frozen.workspace())
            .await?;
        let frozen = agent.frozen_session(principal);
        self.attach(route, principal, device, agent).await?;
        self.journal()
            .set_permissions(principal, frozen.session_id, state.permissions()?)
            .await?;
        self.inherit_model_selection(principal, old, frozen.session_id)
            .await?;
        self.store
            .select(route, principal, frozen.session_id)
            .await?;
        Ok(format!("已开始空白会话。\n设备：{}\n目录：{}\n会话：{}\n权限与模型选择已保留，旧会话历史未删除。直接发送新任务即可。", device.name, frozen.workspace(), frozen.session_id))
    }
}

#[cfg(test)]
mod tests {
    use super::normalize;
    #[test]
    fn aliases_preserve_arguments_and_do_not_interpret_task_text() {
        assert_eq!(
            normalize("/WORKSPACE D:\\My Project\\src"),
            "/工作区 D:\\My Project\\src"
        );
        assert_eq!(normalize("/model vendor/Model-A"), "/模型 vendor/Model-A");
        assert_eq!(normalize("/permissions all"), "/权限 全部");
        assert_eq!(normalize("/result 2"), "/结果 2");
        assert_eq!(normalize("/gateway"), "/网关");
        assert_eq!(normalize("/ui:model Vendor/Model"), "/模型 Vendor/Model");
        assert_eq!(normalize("/core:new"), "/new");
        assert_eq!(normalize("/resume"), "/threads");
        assert_eq!(
            normalize("请解释 /new 和 /compact"),
            "请解释 /new 和 /compact"
        );
    }
}
