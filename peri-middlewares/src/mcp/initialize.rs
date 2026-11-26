use std::path::Path;
use std::sync::Arc;

use rmcp::model::Tool;
use rmcp::service::Peer;
use rmcp::service::RoleClient;
use rmcp::service::ServiceError;

use super::auth_store;
use super::builtin::runtime::BUILTIN_CONVERGE_TIMEOUT;
use super::client;
use super::client::ClientStatus;
use super::client::DiscoveryEvidence;
use super::client::McpClientHandle;
use super::client::McpClientPool;
use super::client::McpInitStatus;
use super::client::OAuthStatus;
use super::client::SystemMcpManifest;
use super::client::HTTP_CONNECT_TIMEOUT;
use super::client::SHUTDOWN_TIMEOUT;
use super::client::STDIO_CONNECT_TIMEOUT;
use super::config::ConfigSource;
use super::config::McpServerConfig;
use super::config::OAuthConfig;
use super::oauth_flow::OAuthFlowEvent;
use super::transport::TransportConfig;
use super::transport::TransportKind;

#[cfg(test)]
#[path = "initialize_test.rs"]
mod tests;

#[cfg(test)]
#[path = "initialize_oauth_test.rs"]
mod oauth_tests;

#[cfg(test)]
#[path = "cache_policy_initialize_test.rs"]
mod cache_policy_tests;

#[cfg(test)]
#[path = "subscription_timeout_test.rs"]
mod subscription_timeout_tests;

/// 三分类超时选择（IF-D1）：由传输形态决定，**禁止**再写成「http / 否则 stdio」的二元
/// 判定——那会让 builtin 复用 stdio 超时，并把失败日志的 `transport` 字段写成事实错误。
pub(super) fn connect_timeout(kind: TransportKind) -> std::time::Duration {
    match kind {
        TransportKind::Stdio => STDIO_CONNECT_TIMEOUT,
        TransportKind::Http => HTTP_CONNECT_TIMEOUT,
        TransportKind::Builtin => super::transport::BUILTIN_CONNECT_TIMEOUT,
    }
}

/// 与 [`connect_timeout`] 同源的日志字段（三分类三取值："stdio" | "http" | "builtin"）。
pub(super) fn transport_label(kind: TransportKind) -> &'static str {
    match kind {
        TransportKind::Stdio => "stdio",
        TransportKind::Http => "http",
        TransportKind::Builtin => "builtin",
    }
}

/// 启动发现的 `tools/list` 来源：System MCP 与 dynamic 连接必须走本次 live round-trip。
///
/// System MCP 的 ready 证据不得来自历史持久缓存——缓存命中只说明「过去某次列出过
/// 这些工具」，不能证明本次启动的 server 仍能列出工具。dynamic 连接同理：按需加载
/// 的 ready 必须由本次连接证明，匿名 server 的版本命中不能替代 `tools/list`。
/// System MCP 的 `system_mcp_tools` 为空数组时同样要走完这次 round-trip（清单可以
/// 是空数组）。其余 eager server 保持既有 cache 策略。
///
/// `Err` 一律表示发现失败：既不等于「服务器没有工具」，也不产生任何 ready 证据。
pub(super) async fn list_discovered_tools(
    pool: &McpClientPool,
    server_name: &str,
    peer: &Peer<RoleClient>,
    config: &McpServerConfig,
) -> Result<Vec<Tool>, ServiceError> {
    if config.system_mcp == Some(true) || config.dynamic {
        peer.list_all_tools().await
    } else {
        pool.list_all_tools_cached_for_startup(server_name, peer, config)
            .await
    }
}

/// 发现尝试的**成功**收口：证据绑定刚提交句柄的代际（`0` 表示未登记，读取方
/// 一律视为无效）。只有真实成功的 live `tools/list` 才走到这里。
pub(super) fn commit_discovery_success(
    pool: &McpClientPool,
    server_name: &str,
    committed: &Arc<McpClientHandle>,
) {
    let generation = pool.handle_generation(committed);
    pool.commit_discovery_evidence(server_name, DiscoveryEvidence::discovered(generation));
}

/// 发现尝试的**失败**收口。
///
/// `insert_failed` / `insert_needs_auth` 已经推进代际，证据必须绑定那个新句柄的
/// 代际，读取方才能把「本代已得出结论」与「仍在进行」区分开。没有句柄可绑定时
/// 清掉旧证据：宁可让等待方按「未完成」等到 deadline，也不留一条无法核对的成功。
pub(super) fn commit_discovery_failure(
    pool: &Arc<McpClientPool>,
    server_name: &str,
    initialize_ok: bool,
) {
    let Some(handle) = pool.client(server_name) else {
        pool.clear_discovery_evidence(server_name);
        return;
    };
    let generation = pool.handle_generation(&handle);
    let evidence = if initialize_ok {
        // initialize 成功但 live `tools/list` 失败：不是「空清单」，也不是完成。
        DiscoveryEvidence::discovery_failed(generation)
    } else {
        DiscoveryEvidence::initialize_failed(generation)
    };
    pool.commit_discovery_evidence(server_name, evidence);
}

/// `tools/list` 失败的统一收口：不提交 `Connected`（那会伪装成「发现完成且无工具」），
/// 改为显式 `Failed` + 本代 `tools_list_ok = false` 的证据。
pub(super) fn fail_tool_discovery(pool: &Arc<McpClientPool>, server_name: &str, error: &str) {
    if pool
        .server_slot(server_name)
        .stopped
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return;
    }
    let reason = format!("工具发现失败: {}", error);
    tracing::warn!(server = %server_name, error = %reason, "MCP tools/list 失败，不发布连接与 ready 证据");
    McpClientPool::insert_failed(pool, server_name, reason);
    commit_discovery_failure(pool, server_name, true);
}

/// 资源清单的降级收口：resources 不是启动准入条件（契约 2 只冻结 transport /
/// initialize / 能力协商 / `tools/list`），连接与工具面保持可用；但解析失败不得
/// 静默写成「成功返回空列表」，必须留下可查的失败记录。
pub(super) fn downgrade_resource_listing(server_name: &str, error: &str) {
    tracing::warn!(
        server = %server_name,
        error = %error,
        "MCP resources/list 失败，本次不发布资源"
    );
}

pub(super) fn fail_workspace_resource_discovery(
    pool: &Arc<McpClientPool>,
    server_name: &str,
    error: &str,
) {
    if pool
        .server_slot(server_name)
        .stopped
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return;
    }
    let reason = format!("资源发现失败: {}", error);
    tracing::warn!(server = %server_name, error = %reason, "Workspace resources/list 失败，不发布连接与 ready 证据");
    McpClientPool::insert_failed(pool, server_name, reason);
    commit_discovery_failure(pool, server_name, true);
}

/// 配置失败的统一发布：面板状态（pool）与 watch 通道同时置 Failed，System 配置
/// 清单同时收口为 `Failed`。
///
/// 只发布失败，不 mark_initialized、不注册 server：失败不能退化成 ready 空配置，
/// 也不能让启动等待方把「配置加载失败」当「还没有 System MCP」睡到超时。
fn publish_config_failure(
    pool: &McpClientPool,
    status_tx: &tokio::sync::watch::Sender<McpInitStatus>,
    message: &str,
) {
    pool.publish_system_manifest(SystemMcpManifest::Failed);
    // 配置失败也是 System 依赖的终态：启动等待方要立即拿到结论，而不是把
    // 「不可用」睡成 10s 上限。
    pool.mark_system_settled();
    let status = McpInitStatus::Failed(message.to_string());
    *pool.init_status.write() = status.clone();
    let _ = status_tx.send(status);
}

include!("initialize_section_2.rs");
