//! Account-scoped MCP servers for cloud sessions.
//!
//! Only public HTTPS endpoints are accepted, host configuration and builtin
//! instances are disabled, and each session's pool contains exactly its
//! authenticated account's servers. MCP tools keep the existing `mcp__` approval
//! policy and are discoverable through ToolSearch.

mod egress;
mod migration;
mod runtime;
mod store;

pub(crate) use migration::migrate;
pub(crate) use runtime::McpCache;
pub(crate) use store::McpServerStore;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum McpConfigError {
    #[error("MCP 服务名只能包含 1-32 个小写字母、数字、- 或 _")]
    Name,
    #[error("MCP 服务名 `{0}` 是保留名称")]
    Reserved(String),
    #[error("MCP 服务 URL 无效：{0}")]
    Url(&'static str),
    #[error("每个账号最多配置 8 个 MCP 服务")]
    Limit,
    #[error("MCP 服务不存在")]
    NotFound,
    #[error("MCP 服务存储不可用")]
    Unavailable,
}

#[cfg(test)]
mod tests;
