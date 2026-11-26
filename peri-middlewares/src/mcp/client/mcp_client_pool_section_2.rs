impl McpClientPool {
    #[must_use]
    pub fn new_pending() -> Self {
        Self::new_pending_with_spawner(super::super::task_scope::McpTaskSpawner::closed())
    }

    #[must_use]
    pub fn new_pending_with_spawner(spawner: super::super::task_scope::McpTaskSpawner) -> Self {
        Self::new_pending_with_spawner_and_profile(
            spawner,
            super::super::apps::McpCapabilityProfile::disabled(),
        )
    }

    #[must_use]
    pub fn new_pending_with_spawner_and_profile(
        spawner: super::super::task_scope::McpTaskSpawner,
        capability_profile: super::super::apps::McpCapabilityProfile,
    ) -> Self {
        Self {
            control: Default::default(),
            catalog_generation: std::sync::atomic::AtomicU64::new(1),
            cache_policy: std::sync::OnceLock::new(),
            workspace_scope: std::sync::OnceLock::new(),
            configuration_snapshot: std::sync::OnceLock::new(),
            frozen_builtin_mode: std::sync::OnceLock::new(),
            session_servers: std::sync::OnceLock::new(),
            credential_client: std::sync::OnceLock::new(),
            builtin_available: std::sync::atomic::AtomicBool::new(true),
            stdio_available: std::sync::atomic::AtomicBool::new(crate::platform::STDIO_TRANSPORT),
            plugin_discovery_available: std::sync::atomic::AtomicBool::new(true),
            plugin_source_failed: std::sync::atomic::AtomicBool::new(false),
            // 默认不关闭：只有会话装配注入的冻结策略才置真（M6）。
            plugin_face_closed: std::sync::atomic::AtomicBool::new(false),
            shared_services: parking_lot::Mutex::new(Vec::new()),
            #[cfg(not(target_os = "emscripten"))]
            processes: parking_lot::Mutex::new(Vec::new()),
            execution_cwd: std::sync::OnceLock::new(),
            lifecycle: std::sync::atomic::AtomicU8::new(0),
            lifecycle_registration: parking_lot::Mutex::new(()),
            #[cfg(test)]
            removal_admission_hits: std::sync::atomic::AtomicUsize::new(0),
            service_shutdown: tokio::sync::Mutex::new(ServiceShutdownState::Idle),
            task_spawner: spawner,
            clients: parking_lot::RwLock::new(HashMap::new()),
            handle_generations: parking_lot::Mutex::new(HashMap::new()),
            next_handle_generation: std::sync::atomic::AtomicU64::new(1),
            next_instance_subscription: std::sync::atomic::AtomicU64::new(1),
            services: parking_lot::Mutex::new(HashMap::new()),
            builtin_server_tasks: parking_lot::Mutex::new(HashMap::new()),
            builtin_context: parking_lot::Mutex::new(BuiltinContextSlot {
                context: None,
                initialize_started: false,
            }),
            configs: parking_lot::RwLock::new(HashMap::new()),
            cache_versions: parking_lot::RwLock::new(HashMap::new()),
            plugin_sources: parking_lot::RwLock::new(HashMap::new()),
            init_status: parking_lot::RwLock::new(McpInitStatus::Pending),
            initialized: std::sync::atomic::AtomicBool::new(false),
            system_settled: std::sync::atomic::AtomicBool::new(false),
            pending_changes: parking_lot::Mutex::new(Vec::new()),
            pending_notifications: parking_lot::Mutex::new(Vec::new()),
            declared_log_levels: parking_lot::RwLock::new(HashMap::new()),
            notifier: parking_lot::RwLock::new(None),
            oauth_event_callback: parking_lot::RwLock::new(None),
            pending_oauth_callbacks: parking_lot::Mutex::new(HashMap::new()),
            active_oauth_flows: parking_lot::Mutex::new(HashMap::new()),
            session_bindings: parking_lot::RwLock::new(session_bindings::SessionBindings::default()),
            resource_cache: super::super::resource_cache::McpResourceCache::new(),
            capability_profile,
            app_binding_leases: Arc::new(super::super::apps::McpAppBindingRegistry::default()),
            system_readiness: SystemReadinessTracker::new(),
            acp_owners: parking_lot::RwLock::new(HashMap::new()),
            acp_connections: parking_lot::RwLock::new(HashMap::new()),
            workspace_remote_started: std::sync::atomic::AtomicBool::new(false),
            lazy_workspace: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn bind_execution_cwd(&self, cwd: &std::path::Path) -> std::io::Result<&std::path::Path> {
        let result = (|| {
            let cwd = std::path::absolute(cwd)?;
            let stored = self.execution_cwd.get_or_init(|| cwd.clone());
            if stored != &cwd {
                return Err(std::io::Error::other(
                    "MCP pool cannot change its execution directory",
                ));
            }
            Ok(stored.as_path())
        })();
        if let Err(error) = &result {
            *self.init_status.write() = McpInitStatus::Failed(error.to_string());
        }
        result
    }

    #[cfg(test)]
    #[must_use]
    pub fn new_empty() -> Self {
        Self::new_empty_with_cache_policy(McpCachePolicy::Enabled)
    }

    /// 空池（跨 crate 集成测试夹具；`test-support` 特性）。
    ///
    /// 与 [`Self::new_empty`] 同形：真实池 + 真实生命周期，只是尚未连接任何 server。
    /// 生产构建不启用该特性，因此不进入生产 API 面。
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn new_empty_for_tests() -> Self {
        Self::new_empty_with_cache_policy(McpCachePolicy::Enabled)
    }

    /// 装入一个**已经建立连接**的客户端句柄（跨 crate 集成测试夹具；`test-support`）。
    ///
    /// 生产路径的句柄注册归 `run_initialize` / MCP 控制面；本入口只让测试能
    /// 构造「真实 peer + 真实池」的夹具，返回该句柄的服务端世代。
    #[cfg(feature = "test-support")]
    pub fn install_client_for_tests(&self, handle: Arc<McpClientHandle>) -> u64 {
        let generation = self.advance_handle_generation(&handle);
        self.clients.write().insert(handle.name.clone(), handle);
        self.bump_catalog_generation();
        generation
    }

    /// 移除一个客户端句柄（跨 crate 集成测试夹具的清理路径；`test-support`）。
    #[cfg(feature = "test-support")]
    pub fn remove_client_for_tests(&self, server_name: &str) {
        self.clients.write().remove(server_name);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn new_empty_with_cache_policy(policy: McpCachePolicy) -> Self {
        let mut pool = Self::new_pending();
        pool.bind_workspace_scope(peri_acp_types::workspace::WorkspaceId::new())
            .unwrap();
        pool.bind_cache_policy(policy).unwrap();
        pool.resource_cache = super::super::resource_cache::McpResourceCache::isolated_for_test();
        pool
    }

    /// 测试夹具：登记一条会话级 ACP 连接（真连接 / service 不参与）。
    ///
    /// 只做 `commit_acp_connection` 的身份记账：归属、声明身份、句柄与句柄代号。
    /// 返回句柄，供测试模拟「同会话换代」时登记第二条句柄。
    #[cfg(test)]
    pub(crate) fn install_acp_connection_for_test(
        self: &Arc<Self>,
        session_id: &str,
        server_id: &str,
        name: &str,
    ) -> Arc<McpClientHandle> {
        let handle = Arc::new(McpClientHandle {
            call_gate: Default::default(),
            name: name.to_string(),
            version: None,
            connected_at: None,
            protocol_version: None,
            cache_version: None,
            peer: None,
            tools: Vec::new(),
            resources: Vec::new(),
            status: ClientStatus::Connected,
            oauth_status: OAuthStatus::default(),
            source: Some(crate::mcp::config::ConfigSource::Acp),
            url: None,
            skills_capable: false,
        });
        self.acp_owners
            .write()
            .insert(name.to_string(), session_id.to_string());
        self.acp_connections.write().insert(
            name.to_string(),
            AcpConnectionDeclaration {
                session_id: session_id.to_string(),
                server_id: server_id.to_string(),
            },
        );
        self.advance_handle_generation(&handle);
        self.clients
            .write()
            .insert(name.to_string(), Arc::clone(&handle));
        handle
    }

    pub fn set_configuration_snapshot(
        &self,
        snapshot: Arc<peri_config::ConfigurationSnapshot>,
    ) -> std::io::Result<()> {
        let context = self.builtin_context.lock();
        if context.initialize_started || !self.is_open() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "configuration snapshot must be bound before MCP initialization",
            ));
        }
        self.configuration_snapshot.set(snapshot).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "MCP configuration snapshot is already bound",
            )
        })
    }

    pub fn set_frozen_builtin_mode(
        &self,
        mode: peri_acp_types::builtin_mcp::BuiltinAssemblyMode,
    ) -> std::io::Result<()> {
        let context = self.builtin_context.lock();
        if context.initialize_started || !self.is_open() {
            return Err(std::io::Error::other(
                "builtin assembly mode must be bound before MCP initialization",
            ));
        }
        self.frozen_builtin_mode.set(mode).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "builtin assembly mode is already bound",
            )
        })
    }

    /// Bind persistent MCP data to the saved Workspace before initialization.
    pub fn bind_workspace_scope(
        &self,
        workspace_id: peri_acp_types::workspace::WorkspaceId,
    ) -> std::io::Result<()> {
        let context = self.builtin_context.lock();
        if context.initialize_started || !self.is_open() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "workspace scope must be bound before MCP initialization",
            ));
        }
        self.workspace_scope.set(workspace_id).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "MCP workspace scope is already bound",
            )
        })
    }

    /// 有效 `workspace` 声明的来源身份（H3/D1：执行环境事实）。
    ///
    /// 判定面覆盖**全部落进本池的来源**：
    /// - 会话声明（`set_session_servers`；新会话来自 ACP 请求，恢复/reopen 来自
    ///   持久 owner，两者都在 `initialize` 前可见）；
    /// - 合并配置（部署/全局/项目/插件层 + builtin overlay，`initialize` 后可见）
    ///   —— 部署/全局配置对 builtin `workspace` 的 HTTP 接管会标记
    ///   [`crate::mcp::config::ConfigSource::WorkspaceRemote`]。
    ///
    /// 会话声明优先（与 `run_initialize` 的合并顺序一致：session servers 覆盖文件
    /// 配置）。`None` = 池中没有任何 `workspace` 条目（无 workspace 面）。
    /// **被 `disabled` 的条目同样返回来源**：禁用只表示能力关闭，不改变
    /// 「该声明的执行环境不在本机」这一事实。
    pub fn workspace_source(&self) -> Option<peri_acp_types::plugin::ConfigSource> {
        self.capability_source("workspace")
    }

    /// ACP session setup declarations are scoped to this pool and override file configuration.
    pub fn set_session_servers(
        &self,
        servers: HashMap<String, McpServerConfig>,
    ) -> std::io::Result<()> {
        let context = self.builtin_context.lock();
        if context.initialize_started || !self.is_open() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "session MCP servers must be bound before initialization",
            ));
        }
        self.session_servers.set(servers).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "session MCP servers already bound",
            )
        })
    }

    pub fn configuration_revision(&self) -> Option<peri_config::ConfigurationRevision> {
        self.configuration_snapshot
            .get()
            .map(|snapshot| snapshot.revision())
    }

    /// Reserve session execution before a tool call can create an MCP Task.
    /// Unconfirmed response loss leaves shutdown incomplete.
    pub(crate) fn begin_external_task_execution(
        &self,
        session_id: &str,
        scope: &str,
    ) -> Result<Box<dyn peri_acp_types::tasks::ExternalExecutionGuard>, String> {
        self.session_bindings
            .read()
            .manager(session_id)
            .ok_or_else(|| "session task manager unavailable".to_owned())?
            .begin_external_execution(scope)
    }

    /// Report whether this session can have asynchronous work owned outside
    /// the trusted Workspace task scope. Read after initialization so failed
    /// and disconnected configured servers remain in the catalog.
    pub async fn has_unsupported_async_task_owner(
        &self,
        trusted_workspace_endpoint: Option<&str>,
    ) -> Result<bool, String> {
        let deadline = peri_time::monotonic_now() + std::time::Duration::from_secs(10);
        while !self.initialized.load(std::sync::atomic::Ordering::Acquire) {
            if peri_time::monotonic_now() >= deadline {
                return Err("MCP owner catalog did not initialize".into());
            }
            peri_time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let configs = self.configs.read();
        Ok(configs.values().any(|config| {
            if config.disabled == Some(true) {
                return false;
            }
            match config.source.as_ref() {
                Some(peri_acp_types::plugin::ConfigSource::Builtin { .. }) => false,
                Some(peri_acp_types::plugin::ConfigSource::WorkspaceRemote) => {
                    config.oauth.is_some() || config.url.as_deref() != trusted_workspace_endpoint
                }
                _ => true,
            }
        }))
    }

    pub(in crate::mcp) fn bind_cache_policy(&self, policy: McpCachePolicy) -> std::io::Result<()> {
        let stored = self.cache_policy.get_or_init(|| policy);
        if *stored != policy {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "MCP pool cannot change its cache policy",
            ));
        }
        Ok(())
    }

    /// 一次性注入 builtin 实例上下文（IF-P3-04 / A33）：首次生效，失败**不覆盖**既有
    /// 上下文（错误只报「为什么这一次没生效」）。
    ///
    /// 拒绝语义（同一短锁内判定，锁内不 await）：
    /// - `initialize` 已开始（[`Self::seal_builtin_context`] 已调用）→
    ///   [`BuiltinContextError::InitializationStarted`]：晚于配置加载窗口的注入无法影响
    ///   本轮实例装配，接受它只会制造「上下文在，但实例是按旧上下文建的」的假象；
    /// - 已有上下文（含传入**同一** `Arc` 的再注入）→
    ///   [`BuiltinContextError::AlreadyInjected`]：注入是宿主组合根的单一动作，重复注入
    ///   只可能是双装配 bug，必须可见而不是静默覆盖。
    pub fn set_builtin_instance_context(
        &self,
        context: Arc<BuiltinInstanceContext>,
    ) -> Result<(), BuiltinContextError> {
        let mut slot = self.builtin_context.lock();
        if slot.initialize_started {
            return Err(BuiltinContextError::InitializationStarted);
        }
        if slot.context.is_some() {
            return Err(BuiltinContextError::AlreadyInjected);
        }
        slot.context = Some(context);
        Ok(())
    }

    /// 封口：标记「`initialize` 已开始」，此后首次注入一律被拒绝（幂等）。
    ///
    /// 调用点只有两个（`initialize` 的形状约束），见 `initialize::run_initialize` 与
    /// `initialize::initialize_config` 的函数体首行。
    pub(crate) fn seal_builtin_context(&self) {
        self.builtin_context.lock().initialize_started = true;
    }

    /// 已注入的上下文（锁内 clone `Arc`；未注入为 `None`）。
    pub(crate) fn builtin_instance_context(&self) -> Option<Arc<BuiltinInstanceContext>> {
        self.builtin_context.lock().context.clone()
    }

    pub(super) fn workspace_source_closed(
        &self,
        closed: &std::collections::BTreeSet<String>,
    ) -> bool {
        let source = self
            .capability_client("workspace")
            .and_then(|handle| handle.source.clone());
        super::super::builtin::is_capability_closed("workspace", source.as_ref(), closed)
    }

    /// builtin 实例的**唯一** spawn 点（IF-P3-04 / A32）：实例解析 → 上下文 → 链路
    /// 装配 → tick。`initialize` 与 `reconnect` 都只调它，不各自读 cwd / 上下文。
    ///
    /// 顺序固定，每一步的错误都不得被后一步掩盖：
    /// 1. **先解析实例**：未注册实例一律 typed `UnknownInstance`（与 stdio / http 路径的
    ///    拒绝语义同源），即使此刻上下文缺失也不改报 `ContextMissing`；
    /// 2. 取上下文：未注入 ⇒ `ContextMissing`（不 panic、不降级成别的传输形态）；
    /// 3. [`super::builtin::runtime::spawn_builtin_transport_with_context`] 装配：
    ///    「已注册但缺该类输入」与「handler 尚未接线」是两个不同的 typed 原因；
    /// 4. tick：仅当注册表名是 `cron`、上下文带 `cron` 输入**且** `tick_enabled` 为真时，
    ///    为**本代** transport spawn 一个 tick（每 `BUILTIN_TICK_INTERVAL` 一次
    ///    `scheduler.lock().tick()`，语义与既有宿主 `HostTaskKind::CronTick` 一致）。
    ///    这是全仓唯一的 tick spawn 点：handler 不持有 tick，tick 随本代监督者一起关闭。
    #[cfg(test)]
    pub(crate) fn spawn_builtin_transport(
        &self,
        instance: &str,
    ) -> Result<BuiltinTransport, BuiltinSpawnError> {
        self.spawn_builtin_transport_with_environment(instance, &std::collections::HashMap::new())
    }

    pub(crate) fn spawn_builtin_transport_with_environment(
        &self,
        instance: &str,
        env: &std::collections::HashMap<String, String>,
    ) -> Result<BuiltinTransport, BuiltinSpawnError> {
        super::super::transport::require_known_builtin_instance(instance).map_err(|_| {
            BuiltinSpawnError::UnknownInstance {
                instance: instance.to_string(),
            }
        })?;
        let context =
            self.builtin_instance_context()
                .ok_or_else(|| BuiltinSpawnError::ContextMissing {
                    instance: instance.to_string(),
                })?;
        let mut transport = super::super::builtin::runtime::spawn_builtin_transport_with_context(
            instance, &context, env,
        )?;
        // 注册表名判定（不写第二张实例名字表）：只有 `cron` 有 tick 驱动语义。
        #[cfg(not(target_os = "emscripten"))]
        if builtin_mcp::find(instance).is_some_and(|registered| registered.name == "cron") {
            if let Some(cron) = context.cron.as_ref().filter(|cron| cron.tick_enabled) {
                let scheduler = Arc::clone(&cron.scheduler);
                transport.tick = Some(TickGuard::spawn(BUILTIN_TICK_INTERVAL, move || {
                    scheduler.lock().tick();
                }));
            }
        }
        // v5 单出口：cron 能力启用且宿主开启 tick 时，仍由 pool 的唯一 spawn 点驱动。
        #[cfg(not(target_os = "emscripten"))]
        if builtin_mcp::is_v5_server_name(instance) {
            let cron_enabled = crate::mcp::builtin::router::RouterTemplate::host_capabilities(
                env,
                &context.closed,
            )
            .iter()
            .any(|capability| capability == "cron");
            if let Some(cron) = context
                .cron
                .as_ref()
                .filter(|cron| cron_enabled && cron.tick_enabled)
            {
                let scheduler = Arc::clone(&cron.scheduler);
                transport.tick = Some(TickGuard::spawn(BUILTIN_TICK_INTERVAL, move || {
                    scheduler.lock().tick();
                }));
            }
        }
        Ok(transport)
    }

    /// 查询指定 server 的插件来源标识，非插件 server 返回 None
    /// key 格式为 `"plugin_name__server_name"`，返回 `"name@marketplace"`
    pub fn plugin_source_of(&self, name: &str) -> Option<String> {
        self.plugin_sources.read().get(name).cloned()
    }

    /// 宿主是否显式声明了 lazy workspace（默认 `false`）。
    pub(crate) fn lazy_workspace(&self) -> bool {
        self.lazy_workspace
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// 远端 workspace 的资源面是否仍在「首个真实调用」之前。
    ///
    /// 只有**宿主显式 opt-in**（[`Self::lazy_workspace`]）且 workspace 来源是
    /// `WorkspaceRemote` 时才成立：cold 面只承诺工具面（`initialize.capabilities`
    /// 只有 `tools`），此时任何 `resources/list` 都会穿透 cold 代理启动真实后端，
    /// 因此启动期资源探测、指令/覆盖读取与 shell 任务扫描都必须跳过。
    ///
    /// 未 opt-in 的普通远端 workspace（任意 ACP 客户端）、本机 builtin `workspace`
    /// 与普通 MCP 都不受影响（返回 false）。
    pub(crate) fn remote_workspace_deferred(&self) -> bool {
        self.lazy_workspace()
            && self.workspace_source()
                == Some(peri_acp_types::plugin::ConfigSource::WorkspaceRemote)
            && !self
                .workspace_remote_started
                .load(std::sync::atomic::Ordering::Acquire)
    }

    /// lazy 远端 workspace 的首次真实 `tools/call` 收口。
    ///
    /// tool bridge 在**成功转发之后**调用：首个调用把「cold」置为「已启动」，
    /// 让后续宿主侧可选探针（shell 任务扫描、git 分支观察等）恢复既有行为；
    /// 同一会话只做一次启动后资源重发现，避免每个 tool call 都追加一次
    /// `resources/list`。
    ///
    /// 只对显式 opt-in 的宿主生效：非 lazy 宿主在启动期已按既有策略完成资源
    /// 发现（失败会拒绝初连），这里不产生额外探测。
    ///
    /// 失败只记 WARN（`downgrade_resource_listing`，与「资源不是启动准入条件」的
    /// 既有语义一致）：session 已创建且工具已在工作，不因一次可选探测失败而崩溃。
    pub(crate) async fn note_remote_workspace_call(&self, handle: &McpClientHandle) {
        if !self.lazy_workspace() {
            return;
        }
        if !matches!(
            handle.source,
            Some(peri_acp_types::plugin::ConfigSource::WorkspaceRemote)
        ) {
            return;
        }
        let first = !self
            .workspace_remote_started
            .swap(true, std::sync::atomic::Ordering::AcqRel);
        if !first {
            return;
        }
        tracing::info!(
            server = %handle.name,
            "远端 workspace 首次真实调用已转发；开始启动后资源重发现"
        );
        let Some(peer) = handle.peer.as_ref() else {
            tracing::warn!(
                server = %handle.name,
                "远端 workspace 已启动但 peer 不可用；跳过启动后资源重发现"
            );
            return;
        };
        match peri_time::timeout(
            REMOTE_WORKSPACE_RESOURCE_PROBE_TIMEOUT,
            self.list_all_resources_cached(&handle.name, peer),
        )
        .await
        {
            Ok(Ok(resources)) => tracing::info!(
                server = %handle.name,
                resources = resources.len(),
                "远端 workspace 启动后资源重发现完成"
            ),
            Ok(Err(error)) => super::super::initialize::downgrade_resource_listing(
                &handle.name,
                &error.to_string(),
            ),
            Err(_) => tracing::warn!(
                server = %handle.name,
                timeout_secs = REMOTE_WORKSPACE_RESOURCE_PROBE_TIMEOUT.as_secs(),
                "远端 workspace 启动后资源重发现超时；本次不发布资源"
            ),
        }
    }

    /// MetaHarness 段落覆盖读取（J6）：只认真实 builtin `workspace` 实例。
    ///
    /// - X7：来源白名单按实例身份（`ConfigSource::Builtin { instance: "workspace" }`），
    ///   外部 server 的同 scheme 资源一律不进入扫描（不按 scheme 信任）；
    /// - X8：关闭集命中的实例直接返回空批（覆盖不可用 ⇒ 宿主保持内置段落；
    ///   无磁盘兜底，宿主已无 FS 扫描点）；
    /// - 未连接 / 未注入上下文同样返回空批，由宿主 X8 语义处理。
    pub async fn read_builtin_workspace_meta(
        &self,
        enabled_sections: &std::collections::HashSet<String>,
    ) -> Result<HashMap<String, String>, String> {
        let closed = self
            .builtin_instance_context()
            .map(|context| context.closed.clone())
            .unwrap_or_default();
        if self.workspace_source_closed(&closed) {
            tracing::warn!("meta_harness: builtin workspace is closed; keeping builtin sections");
            return Ok(HashMap::new());
        }
        // S9a3a：lazy 远端 workspace 的 cold 面没有资源能力（只宣告 tools）。在首个
        // 真实调用之前读取覆盖会把「可选探测」变成一次真实启动，因此按
        // 「覆盖不可得 ⇒ 保持内置段落」（X8）处理，而不是去探测。
        if self.remote_workspace_deferred() {
            tracing::info!(
                "meta_harness: remote workspace is still cold; keeping builtin sections until first real call"
            );
            return Ok(HashMap::new());
        }
        let Some(handle) = self.capability_client("workspace") else {
            return Ok(HashMap::new());
        };
        if !matches!(handle.status, ClientStatus::Connected)
            || !super::super::builtin::is_workspace_source(handle.source.as_ref())
        {
            return Ok(HashMap::new());
        }
        let Some(peer) = handle.peer.as_ref() else {
            return Ok(HashMap::new());
        };
        let server = handle.name.as_str();
        let resources = self
            .list_all_resources_cached(server, peer)
            .await
            .map_err(|error| format!("resources/list failed: {error}"))?;
        let mut docs = HashMap::new();
        let mut seen = HashSet::new();
        for resource in resources {
            let Some(meta) = peri_acp_types::workspace_resources::parse_meta_uri(&resource.uri)
            else {
                continue;
            };
            if !enabled_sections.contains(&meta.section_id)
                || !peri_acp_types::meta_harness::SECTION_IDS.contains(&meta.section_id.as_str())
            {
                continue;
            }
            if !seen.insert(meta.section_id.clone()) {
                return Err(format!(
                    "duplicate workspace prompt override for section {} from {server}",
                    meta.section_id
                ));
            }
            let (result, ticket) = self
                .read_resource_cached(server, &resource.uri, peer)
                .await
                .map_err(|error| format!("resources/read failed: {error}"))?;
            self.cache_verified_resource(server, ticket, &result).await;
            if let Some(text) = result.contents.iter().find_map(|content| match content {
                ResourceContents::TextResourceContents { text, .. } => Some(text.clone()),
                _ => None,
            }) {
                docs.insert(meta.section_id, text);
            }
        }
        Ok(docs)
    }

    /// 冻结期项目指令读取（W5/E15）：只认真实 builtin `workspace` 实例。
    ///
    /// 身份与关闭口径与 [`Self::read_builtin_workspace_meta`] 同源（实例绑定 +
    /// Connected + A24 关闭集）；`disableBundledSkills` 等 skill 面开关不适用于
    /// 指令面。
    ///
    /// 失败语义（X5，受限 Peri profile）：
    /// - 未装配 / 实例被关闭 / 未连接 / 非本机 builtin 实例 ⇒ `Ok((None, None))`
    ///   ：指令面不适用，**不是失败**，也不回落磁盘（X4/J5）；
    /// - 实例健康但 `resources/list` / `resources/read` 失败 ⇒ `Err`：system
    ///   已声明且被选中的投递失败，调用方按 J2 补偿 fail-closed；
    /// - 文档不存在（`resources/list` 未列 main/local）⇒ `Ok(None)`，不是错误。
    ///
    /// 返回 `(main, local)`：main 为 import 展开后的正文（provider 侧解析），
    /// local 为原文；两者都可为 `None`。
    pub async fn read_builtin_workspace_instructions(
        &self,
    ) -> Result<(Option<String>, Option<String>), String> {
        let closed = self
            .builtin_instance_context()
            .map(|context| context.closed.clone())
            .unwrap_or_default();
        if self.workspace_source_closed(&closed) {
            tracing::debug!(
                "instructions: builtin workspace is closed; instruction face stays unavailable"
            );
            return Ok((None, None));
        }
        // S9a3a：lazy 远端 workspace 在首个真实调用前只有 cold 工具面（资源能力未宣告）。
        // 指令面按「不适用」返回空，而不是用一次 `resources/list` 启动真实后端；
        // 冻结的 system prompt 因此不含远端工作区指令（本机 builtin 不受影响）。
        if self.remote_workspace_deferred() {
            tracing::info!(
                "instructions: remote workspace is still cold; instruction face stays unavailable until first real call"
            );
            return Ok((None, None));
        }
        let Some(handle) = self.capability_client("workspace") else {
            return Ok((None, None));
        };
        if !matches!(handle.status, ClientStatus::Connected)
            || !super::super::builtin::is_workspace_source(handle.source.as_ref())
        {
            return Ok((None, None));
        }
        let Some(peer) = handle.peer.clone() else {
            return Err(
                "builtin workspace instruction face is connected but the peer is unavailable"
                    .to_string(),
            );
        };
        // M9：`resources/list` 只是发现提示，**不证明正文已锁定**。首次采集
        // 出现不一致（已列出的指令资源读取失败）时只做一次有界重采集；仍不一致
        // 就返回明确的可重试准入错误，不提交半套冻结数据。
        let server = handle.name.clone();
        collect_with_single_recollect("workspace instruction face", || {
            self.collect_instruction_documents(&server, &peer)
        })
        .await
    }

    /// 一次 list→read 采集（main / local）；任一已列出资源读取失败即返回 `Err`。
    pub(super) async fn collect_instruction_documents(
        &self,
        server: &str,
        peer: &rmcp::service::Peer<rmcp::service::RoleClient>,
    ) -> Result<(Option<String>, Option<String>), String> {
        use peri_acp_types::workspace_resources;
        use peri_acp_types::workspace_resources::INSTRUCTION_LOCAL_URI;
        use peri_acp_types::workspace_resources::INSTRUCTION_MAIN_URI;
        let resources = self
            .list_all_resources_cached(server, peer)
            .await
            .map_err(|error| format!("resources/list failed: {error}"))?;
        let mut main: Option<String> = None;
        let mut local: Option<String> = None;
        for resource in resources {
            // 只读固定的两条指令 URI（不按 scheme 泛化：其余 scheme 不在此面）。
            let uri = resource.uri.as_str();
            let is_main = uri == INSTRUCTION_MAIN_URI;
            let is_local = uri == INSTRUCTION_LOCAL_URI;
            if !is_main && !is_local {
                continue;
            }
            if workspace_resources::parse_instruction_uri(uri).is_none() {
                continue;
            }
            let (result, ticket) =
                self.read_resource_cached(server, uri, peer)
                    .await
                    .map_err(|error| {
                        format!("listed instruction resource {uri} could not be read: {error}")
                    })?;
            self.cache_verified_resource(server, ticket, &result).await;
            let Some(text) = result.contents.iter().find_map(|content| match content {
                ResourceContents::TextResourceContents { text, .. } => Some(text.clone()),
                _ => None,
            }) else {
                continue;
            };
            match (is_main, is_local) {
                (true, _) => main = Some(text),
                (_, true) => local = Some(text),
                _ => {}
            }
        }
        Ok((main, local))
    }

    /// 冻结期技能清单快照（按 **skills 能力**聚合，不按 server 名）。
    ///
    /// 遍历本会话可见、`Connected`、声明了 SEP-2640 skills 扩展的句柄；每个
    /// server 独立并发调用 `skills/list`（共享 10s 预算）。条目名已是
    /// `mcp__<server>__<skill>`，不同 server 的同名技能天然区分；同一 server
    /// 内冲突由 [`crate::mcp::skill_discovery::snapshot_via_skills_list`] 消歧。
    ///
    /// 失败语义：
    /// - 未声明 skills / 不在本会话可见范围 / 被 A24 关闭集命中 ⇒ 跳过（能力
    ///   不适用不是失败）；
    /// - 非 system server 读取失败 ⇒ `tracing::warn!` 并继续，保留其他 server
    ///   的条目（常规发现任务会独立把该 server 标成 Failed）；
    /// - host-assigned system workspace（builtin `workspace` 或
    ///   `WorkspaceRemote`）读取失败 ⇒ `Err`，调用方 fail-closed。
    pub async fn read_skills_catalog(
        &self,
        session_id: &str,
    ) -> Result<Vec<SkillMetadata>, String> {
        let closed = self
            .builtin_instance_context()
            .map(|context| context.closed.clone())
            .unwrap_or_default();
        let handles: Vec<_> = self
            .clients_visible_to(Some(session_id))
            .into_iter()
            .filter(|handle| handle.skills_capable)
            .filter(|handle| matches!(handle.status, ClientStatus::Connected))
            .filter(|handle| {
                if peri_acp_types::builtin_mcp::is_v5_server_name(&handle.name) {
                    // 技能面由 workspace 能力承载；能力关闭时整条 peri 连接仍在，
                    // 但不得再读 skills。
                    !closed.contains("workspace")
                } else {
                    !super::super::builtin::is_closed_source(
                        &handle.name,
                        handle.source.as_ref(),
                        &closed,
                    )
                }
            })
            .collect();
        if handles.is_empty() {
            return Ok(Vec::new());
        }
        let cancel = tokio_util::sync::CancellationToken::new();
        let reads = handles.into_iter().map(|handle| {
            let server = handle.name.clone();
            let system = super::super::builtin::is_workspace_source(handle.source.as_ref());
            let peer = handle.peer.clone();
            let cancel = cancel.clone();
            async move {
                let Some(peer) = peer else {
                    return (
                        server,
                        system,
                        Err("skills declared but the peer is unavailable".to_string()),
                    );
                };
                let outcome = peri_time::timeout(
                    std::time::Duration::from_secs(10),
                    super::super::skill_discovery::snapshot_via_skills_list(peer, &server, cancel),
                )
                .await;
                match outcome {
                    Ok(result) => (server, system, result),
                    Err(_) => (
                        server,
                        system,
                        Err("skills/list timed out after 10s".to_string()),
                    ),
                }
            }
        });
        let results = futures::future::join_all(reads).await;
        let mut catalog: Vec<SkillMetadata> = Vec::new();
        let mut seen = HashSet::new();
        let mut system_failures = Vec::new();
        for (server, system, result) in results {
            match result {
                Ok(entries) => {
                    for entry in entries {
                        if seen.insert(entry.name.clone()) {
                            catalog.push(entry);
                        }
                    }
                }
                Err(error) if system => system_failures.push(format!("{server}: {error}")),
                Err(error) => tracing::warn!(
                    server,
                    %error,
                    "MCP skills/list failed; continuing without this server's skills"
                ),
            }
        }
        if !system_failures.is_empty() {
            return Err(format!(
                "system workspace skills face failed: {}",
                system_failures.join("; ")
            ));
        }
        catalog.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(catalog)
    }
}
