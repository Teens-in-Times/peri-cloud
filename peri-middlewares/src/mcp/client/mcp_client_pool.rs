#[cfg(not(target_os = "emscripten"))]
use super::builtin_mcp;
#[cfg(not(target_os = "emscripten"))]
use super::process;
#[cfg(test)]
use super::OAuthStatus;
use super::{
    fmt, session_bindings, Arc, BuiltinContextError, BuiltinInstanceContext, BuiltinSpawnError,
    BuiltinTransport, ClientStatus, Debug, Formatter, HashMap, HashSet, McpCachePolicy,
    McpClientHandle, McpConnectionKey, McpInitStatus, McpServerConfig, McpServiceOwner,
    McpServiceWrapper, OAuthFlowEvent, OAuthFlowKey, PendingMcpNotification, PendingOAuthCallback,
    ResourceContents, ServiceShutdownState, SkillMetadata, SystemReadinessTracker,
};
#[cfg(not(target_os = "emscripten"))]
use super::{TickGuard, BUILTIN_TICK_INTERVAL};

/// 远端 workspace 启动后一次性资源重发现的上界（S9a3a）。
///
/// 真实后端就在本机/沙箱内，正常是毫秒级；上界只兜底「cold 代理卡住」，
/// 超时按可见失败记录，不阻塞已创建的 session。
const REMOTE_WORKSPACE_RESOURCE_PROBE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(5);

/// MCP 客户端连接池
pub struct McpClientPool {
    pub(crate) control: super::super::control::PoolControl,
    pub(crate) catalog_generation: std::sync::atomic::AtomicU64,
    pub(super) cache_policy: std::sync::OnceLock<McpCachePolicy>,
    pub(super) workspace_scope: std::sync::OnceLock<peri_acp_types::workspace::WorkspaceId>,
    pub(in crate::mcp) configuration_snapshot:
        std::sync::OnceLock<Arc<peri_config::ConfigurationSnapshot>>,
    pub(in crate::mcp) frozen_builtin_mode:
        std::sync::OnceLock<peri_acp_types::builtin_mcp::BuiltinAssemblyMode>,
    pub(in crate::mcp) session_servers: std::sync::OnceLock<HashMap<String, McpServerConfig>>,
    pub(super) credential_client:
        std::sync::OnceLock<super::super::auth_store::OAuthCredentialClient>,
    pub(in crate::mcp) builtin_available: std::sync::atomic::AtomicBool,
    pub(in crate::mcp) stdio_available: std::sync::atomic::AtomicBool,
    pub(in crate::mcp) plugin_discovery_available: std::sync::atomic::AtomicBool,
    pub(in crate::mcp) plugin_source_failed: std::sync::atomic::AtomicBool,
    /// 插件来源闭合位（M6）：会话装配从冻结/session-local 策略派生后注入，
    /// 与 `plugin_discovery_available`（部署是否具备插件能力）是两个事实。
    pub(in crate::mcp) plugin_face_closed: std::sync::atomic::AtomicBool,
    pub(super) shared_services: parking_lot::Mutex<Vec<Arc<McpServiceOwner>>>,
    /// Includes failed handshakes until their actual process tree and stderr have drained.
    #[cfg(not(target_os = "emscripten"))]
    pub(super) processes: parking_lot::Mutex<Vec<Arc<process::McpProcessOwner>>>,
    /// Static transports reconnect in the same session directory used for initial discovery.
    pub(crate) execution_cwd: std::sync::OnceLock<std::path::PathBuf>,
    /// Pool-wide admission gate. 0=open, 1=closing, 2=closed.
    pub(super) lifecycle: std::sync::atomic::AtomicU8,
    pub(crate) lifecycle_registration: parking_lot::Mutex<()>,
    /// 测试观测点：`remove_server` 触及准入边界的次数（仅测试构建）。
    ///
    /// 计数发生在获取准入锁之前，测试据此确认卸载任务已运行到共享准入，
    /// 而不是把「超时未完成」当作阻塞证据。
    #[cfg(test)]
    pub(crate) removal_admission_hits: std::sync::atomic::AtomicUsize,
    /// Pool-owned terminal service-close transaction. Awaiting a borrowed
    /// handle is cancellation-safe: a dropped waiter cannot detach the worker
    /// or the drained services it owns.
    pub(super) service_shutdown: tokio::sync::Mutex<ServiceShutdownState>,
    pub(crate) task_spawner: super::super::task_scope::McpTaskSpawner,
    pub(crate) clients: parking_lot::RwLock<HashMap<String, Arc<McpClientHandle>>>,
    pub(super) handle_generations:
        parking_lot::Mutex<HashMap<String, Vec<(std::sync::Weak<McpClientHandle>, u64)>>>,
    pub(super) next_handle_generation: std::sync::atomic::AtomicU64,
    /// 宿主面按实例资源订阅的池内序号（`instance_face`；唯一化同一实例/URI 的多次
    /// 订阅，不承载业务语义）。
    pub(in crate::mcp) next_instance_subscription: std::sync::atomic::AtomicU64,
    pub(crate) services: parking_lot::Mutex<HashMap<String, McpServiceWrapper>>,
    /// builtin 实例的**代监督者**表（键 = server name，与 `services` 同期登记 / 移除）。
    ///
    /// 值是关闭所有权而不是裸 server task：builtin 的 server 半边是本进程内的 task，
    /// 关闭语义必须显式（有界等待 → 未收敛才 abort），且顺序固定（先停 tick 再收敛
    /// server task）。不能假设「client service 关闭后它自己会退出」（spike Q2 才有该结论）。
    /// 冻结：wave 1 **不**新增 `McpTaskKey` 变体，本表独立于 keyed task 作用域。
    pub(crate) builtin_server_tasks: parking_lot::Mutex<
        HashMap<String, super::super::builtin::runtime::BuiltinInstanceSupervisor>,
    >,
    /// builtin 实例上下文槽（IF-P3-04 / A33）。
    ///
    /// 语义是**一次性注入**：宿主装配（`peri-acp`）在 `run_initialize` 之前注入，首次生效；
    /// 重复注入（含同一 `Arc` 再注入）与「initialize 已开始」后的晚注入都返回 typed 错误。
    /// 上下文与「initialize 已开始」标志由**同一把短锁**保护——锁内只做读判定与写入、
    /// 禁止 await，因此两个竞争注入恰好一个成功，且晚注入的判定与 `initialize` 封口
    /// 之间不存在窗口（`OnceLock::set` 表达不了后半条）。
    pub(crate) builtin_context: parking_lot::Mutex<BuiltinContextSlot>,
    pub(crate) configs: parking_lot::RwLock<HashMap<String, McpServerConfig>>,
    pub(crate) cache_versions: parking_lot::RwLock<HashMap<String, String>>,
    /// 插件来源旁路表：key 为 server name（如 `"plugin:p1:srv1"`），value 为 `"name@marketplace"`
    pub(crate) plugin_sources: parking_lot::RwLock<HashMap<String, String>>,
    /// 初始化阶段内部存储（M-TUI 收口：TUI 不再持有 watch channel，`mcp/list`
    /// 命令面经 `McpPoolPort::snapshot` 读取；`run_initialize` 与外部
    /// `status_tx` 同步更新）。
    pub(crate) init_status: parking_lot::RwLock<McpInitStatus>,
    /// 初始化是否已完成。完成前发生的状态写入**不**产生上下线通知——
    /// 会话首 turn 的 `first_turn_reminder` 概览已覆盖初始连接结果，避免与
    /// 逐台上线事件重复（初始化未完成时，迟到的连接成功自然成为运行中变化）。
    pub(crate) initialized: std::sync::atomic::AtomicBool,
    /// System MCP（启动依赖）是否已处理完。普通 MCP 不是启动依赖：会话创建
    /// 只等这个位，不等整池 `initPhase`，否则不可达的普通 transport 会拖住
    /// 首个会话（每个 HTTP 连接失败要等满超时）。
    pub(crate) system_settled: std::sync::atomic::AtomicBool,
    /// 运行中状态变化的待注入文本缓冲（McpMiddleware::before_model drain 后
    /// 以 Info 消息推送进模型上下文；全局缓冲，任一会话消费一次即清空）。
    pub(crate) pending_changes: parking_lot::Mutex<Vec<String>>,
    /// MCP 自定义通知的待注入缓冲（与 `pending_changes` 同点位 drain；
    /// 全局缓冲，任一会话消费一次即清空）。
    pub(crate) pending_notifications: parking_lot::Mutex<Vec<PendingMcpNotification>>,
    /// `notifications/message` 门控：各 server 在请求 `_meta` 中声明过的 logLevel。
    ///
    /// Peri 不主动声明该键；仅当调用方已声明时 `notifications/message` 才有可见出口。
    #[allow(deprecated)]
    pub(crate) declared_log_levels: parking_lot::RwLock<HashMap<String, rmcp::model::LoggingLevel>>,
    /// 状态变化通知回调（装配时注入；发布 system-notification 给 TUI 通知面）。
    pub(super) notifier: parking_lot::RwLock<Option<Arc<dyn Fn(&str) + Send + Sync>>>,
    /// OAuth 流程事件回调（装配时注入；`AuthorizationNeeded` 需把
    /// `callback_tx` 注册进 `pending_oauth_callbacks` 供授权码回传 RPC 投递，
    /// 其余事件转发为 ACP `oauth-needed` / `oauth-completed` / `oauth-failed`）。
    pub(super) oauth_event_callback:
        parking_lot::RwLock<Option<Arc<dyn Fn(OAuthFlowEvent) + Send + Sync>>>,
    /// 待完成 OAuth 授权的回调通道。物理连接与 flow identity 共同定位，
    /// dynamic 路径不得降维为裸 server name。
    pub(super) pending_oauth_callbacks:
        parking_lot::Mutex<HashMap<OAuthFlowKey, PendingOAuthCallback>>,
    /// 每个 scoped connection 最多一个活跃 OAuth flow。
    pub(super) active_oauth_flows: parking_lot::Mutex<HashMap<McpConnectionKey, String>>,
    /// subscriptions/listen 会话 inbox 注册表（session_id → InboxHandle）。
    /// SessionManager（peri-acp）经 `McpSubscriptionPort` 注册；订阅通知到达
    /// 时向全部注册 inbox 推送 Defer 消息并唤醒 idle agent。
    pub(crate) session_bindings: parking_lot::RwLock<session_bindings::SessionBindings>,
    /// 跨进程的 MCP Resource Cache；是否写入由响应 scope 与安全上下文共同决定。
    pub(crate) resource_cache: super::super::resource_cache::McpResourceCache,
    /// 进程启动时冻结的 deployment capability profile；初始连接和重连复用。
    pub(crate) capability_profile: super::super::apps::McpCapabilityProfile,
    /// 初始模型 MCP tool invocation 签发、`peri/mcp/open` 单次消费的租约。
    pub(crate) app_binding_leases: Arc<super::super::apps::McpAppBindingRegistry>,
    /// System MCP 启动准入事实（IF-M3）：配置清单状态 + 每台 server 的本代发现
    /// 证据 + 独立 watch revision。所有写入都必须经由成功/失败的生产路径。
    pub(crate) system_readiness: SystemReadinessTracker,
    /// 会话级 ACP（MCP over ACP）连接归属：池内 server name → 所属 session id。
    /// 无条目的 server 对所有会话可见（配置来源与 dynamic 投影的既有语义）；
    /// 有条目的仅在归属会话内可见（工具桥接与状态面据此过滤）。
    pub(crate) acp_owners: parking_lot::RwLock<HashMap<String, String>>,
    /// 会话级 ACP 连接的**声明身份**（M7）：池内 server name → `serverId` + 归属。
    /// 与 `acp_owners` 同批写入、同批移除；持久化 cache 只按它 + 当前句柄代号
    /// 计算 origin，凭据从不进入本结构。
    pub(crate) acp_connections: parking_lot::RwLock<HashMap<String, AcpConnectionDeclaration>>,
    /// 远端 workspace（`ConfigSource::WorkspaceRemote`）首次真实 `tools/call` 是否已发生。
    ///
    /// 只对**显式**声明 lazy workspace 的宿主有意义（[`Self::lazy_workspace`]）：
    /// 远端 cold 面的 `initialize` 只宣告 tools（资源面要真实后端才有），因此
    /// 启动期的 `resources/list`、指令/覆盖读取与 shell 任务扫描都会把「可选探测」
    /// 变成一次真实启动。首个真实调用之前这些探针跳过；调用成功后置真，
    /// 并触发一次启动后资源重发现。
    pub(crate) workspace_remote_started: std::sync::atomic::AtomicBool,
    /// 宿主显式声明的 lazy workspace（默认 `false` = 既有语义）。
    ///
    /// `true` 且 workspace 来源是 `WorkspaceRemote` 时，启动期资源/指令/Git/任务
    /// 探测才延后到首个真实 `tools/call`。普通 ACP 客户端的远端 workspace
    /// **不**默认 lazy：资源发现失败仍拒绝初连。Swarm 的 Bun cold 代理是唯一
    /// 显式 opt-in 方（`AcpChild` 装配）。
    pub(in crate::mcp) lazy_workspace: std::sync::atomic::AtomicBool,
}

impl Debug for McpClientPool {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpClientPool")
            .finish_non_exhaustive()
    }
}

/// 会话级 ACP 连接的声明事实（非凭据）：`server_id` 是 client 在会话 setup 中
/// 声明的 `serverId`（`mcp/message` 的寻址依据），随会话关闭失效。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AcpConnectionDeclaration {
    pub(crate) session_id: String,
    pub(crate) server_id: String,
}

/// builtin 实例上下文槽（A33）：上下文与「initialize 已开始」标志由**同一短锁**保护
/// （锁内禁 await）。
///
/// 两件事必须同锁：否则「判重复 → 写上下文」与「封口 → 判晚注入」之间存在竞态，晚注入
/// 可能挤进已经开始的 initialize。`initialize_started` 一旦置真即不可回退（seal 幂等，
/// 本类型不提供解封口入口）。
pub(crate) struct BuiltinContextSlot {
    /// 已注入的上下文；`None` = 尚未注入（**不是**「不需要上下文」）。
    pub(super) context: Option<Arc<BuiltinInstanceContext>>,
    /// `initialize` 是否已开始（seal 点见 `initialize::run_initialize` /
    /// `initialize::initialize_config` 的函数体首行）。
    pub(in crate::mcp) initialize_started: bool,
}

pub(crate) const STDIO_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub(crate) const HTTP_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub(crate) const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Subscription handshakes are best-effort; a slow server must not block connection readiness.
pub(crate) const SUBSCRIPTION_LISTEN_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(10);

/// One `tasks/cancel` RPC. Cancellation is cooperative and the ack is not the
/// task's terminal state; a silent owner must not hold the caller forever.
/// rmcp's native request timeout also emits `notifications/cancelled`.
pub(crate) const MCP_TASK_CANCEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 有界重采集（M9，list→read TOCTOU）：首次采集失败后**只**重采集一次。
///
/// 仍不一致时返回显式的内容准入错误（可重试），调用方据此 fail-closed——
/// 不把半套采集结果当作冻结数据提交。`context` 是错误文案里的面名称，
/// 不含正文与主机路径。
pub(super) async fn collect_with_single_recollect<T, F, Fut>(
    context: &str,
    mut collect: F,
) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    match collect().await {
        Ok(value) => Ok(value),
        Err(first) => {
            tracing::warn!(
                error = %first,
                face = context,
                "首次采集不一致，有界重采集一次（list→read TOCTOU）"
            );
            collect().await.map_err(|second| {
                format!(
                    "{context} stayed inconsistent after one re-collection ({second}); \
                     retry the session admission"
                )
            })
        }
    }
}

include!("mcp_client_pool_section_2.rs");
