impl ServerHandler for BuiltinRouter {
    fn get_info(&self) -> ServerConfig {
        let mut capabilities = ServerCapabilities::default();
        for entry in self.ready_entries() {
            capabilities = merge_capabilities(capabilities, &entry.handler.get_info().capabilities);
        }
        ServerConfig::new(capabilities).with_server_info(Implementation::new(
            ROUTER_SERVER_NAME,
            env!("CARGO_PKG_VERSION"),
        ))
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        for entry in self.ready_entries() {
            entry.handler.on_initialized(context.clone()).await;
        }
    }

    async fn on_cancelled(
        &self,
        notification: CancelledNotificationParam,
        context: NotificationContext<RoleServer>,
    ) {
        for entry in self.ready_entries() {
            entry
                .handler
                .on_cancelled(notification.clone(), context.clone())
                .await;
        }
    }

    async fn on_progress(
        &self,
        notification: ProgressNotificationParam,
        context: NotificationContext<RoleServer>,
    ) {
        for entry in self.ready_entries() {
            entry
                .handler
                .on_progress(notification.clone(), context.clone())
                .await;
        }
    }

    async fn on_roots_list_changed(&self, context: NotificationContext<RoleServer>) {
        for entry in self.ready_entries() {
            entry.handler.on_roots_list_changed(context.clone()).await;
        }
    }

    async fn on_custom_notification(
        &self,
        notification: CustomNotification,
        context: NotificationContext<RoleServer>,
    ) {
        for entry in self.ready_entries() {
            entry
                .handler
                .on_custom_notification(notification.clone(), context.clone())
                .await;
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        self.require_any_ready()?;
        let mut tools = Vec::new();
        for entry in self.ready_entries() {
            let result = match guarded(
                entry.declared.name,
                entry.handler.list_tools(None, context.clone()),
            )
            .await
            {
                Ok(result) => result,
                Err(error) => {
                    tracing::warn!(
                        capability = entry.declared.name,
                        %error,
                        "skipping capability tools after a failed list"
                    );
                    continue;
                }
            };
            for mut tool in result.tools {
                let Some(declared) = entry
                    .declared
                    .tools
                    .iter()
                    .find(|declared| declared.original_name == tool.name)
                else {
                    tracing::warn!(
                        capability = entry.declared.name,
                        tool = %tool.name,
                        "builtin handler listed a tool missing from the capability registry"
                    );
                    continue;
                };
                if !declared.model_visible {
                    continue;
                }
                // wire 目录保留**原始名**：模型面 `mcp__peri__*` 由 Peri 的工具桥
                // 统一生成（单出口协议层不冒充客户端命名空间）。
                tool.name = declared.original_name.into();
                tools.push(tool);
            }
        }
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        self.require_any_ready()?;
        // 执行身份是 `(capability, original_name)`；wire 目录与工具桥都使用原始名。
        // 同时接受 v5 模型面名字（`mcp__peri__*`），便于直接使用协议名的客户端；
        // 迁移期旧名仍由 `resolve_model_tool` 显式拒绝。
        let routed = match capability_of_original_tool(&request.name) {
            Some((capability, tool)) => RoutedTool {
                capability: capability.name,
                tool,
            },
            None => resolve_model_tool(&request.name).map_err(|error| {
                McpError::new(ErrorCode::METHOD_NOT_FOUND, error.to_string(), None)
            })?,
        };
        let entry = self.require_ready(routed.capability)?;
        let mut request = request;
        request.name = routed.tool.original_name.into();
        let response = guarded(
            entry.declared.name,
            entry.handler.call_tool(request, context),
        )
        .await?;
        self.register_task(entry.declared.name, &response)?;
        Ok(response)
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        self.require_any_ready()?;
        let mut resources: Vec<Resource> = Vec::new();
        for entry in self.ready_entries() {
            let result = match guarded(
                entry.declared.name,
                entry.handler.list_resources(None, context.clone()),
            )
            .await
            {
                Ok(result) => result,
                Err(error) => {
                    tracing::warn!(
                        capability = entry.declared.name,
                        %error,
                        "skipping capability resources after a failed list"
                    );
                    continue;
                }
            };
            for resource in result.resources {
                if entry.accepts_resource(&resource.uri)
                    && !resources
                        .iter()
                        .any(|existing| existing.uri == resource.uri)
                {
                    resources.push(resource);
                }
            }
        }
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        self.require_any_ready()?;
        let mut templates: Vec<rmcp::model::ResourceTemplate> = Vec::new();
        for entry in self.ready_entries() {
            let result = match guarded(
                entry.declared.name,
                entry.handler.list_resource_templates(None, context.clone()),
            )
            .await
            {
                Ok(result) => result,
                Err(error) => {
                    tracing::warn!(
                        capability = entry.declared.name,
                        %error,
                        "skipping capability resource templates after a failed list"
                    );
                    continue;
                }
            };
            for template in result.resource_templates {
                let declared = entry
                    .declared
                    .resources
                    .resource_templates
                    .contains(&template.uri_template.as_str());
                if declared
                    && !templates
                        .iter()
                        .any(|existing| existing.uri_template == template.uri_template)
                {
                    templates.push(template);
                }
            }
        }
        Ok(ListResourceTemplatesResult::with_all_items(templates))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        self.require_any_ready()?;
        let entry = self.route_resource(&request.uri)?;
        guarded(
            entry.declared.name,
            entry.handler.read_resource(request, context),
        )
        .await
    }

    async fn read_resource_directory(
        &self,
        request: ReadResourceDirectoryRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceDirectoryResult, McpError> {
        self.require_any_ready()?;
        let entry = self.route_resource(&request.uri)?;
        guarded(
            entry.declared.name,
            entry.handler.read_resource_directory(request, context),
        )
        .await
    }

    #[allow(deprecated)]
    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.require_any_ready()?;
        let entry = self.route_resource(&request.uri)?;
        guarded(
            entry.declared.name,
            entry.handler.subscribe(request, context),
        )
        .await
    }

    #[allow(deprecated)]
    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.require_any_ready()?;
        let entry = self.route_resource(&request.uri)?;
        guarded(
            entry.declared.name,
            entry.handler.unsubscribe(request, context),
        )
        .await
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        let mut uris: Vec<String> = Vec::new();
        let mut accepts_tasks = false;
        for entry in self.ready_entries() {
            accepts_tasks |= entry.supports_tasks();
            for uri in entry.declared.resources.default_subscriptions {
                if !uris.iter().any(|existing| existing == uri) {
                    uris.push((*uri).to_owned());
                }
            }
        }
        if uris.is_empty() && !accepts_tasks {
            return None;
        }
        let mut builder = SubscriptionFilter::builder();
        if !uris.is_empty() {
            builder = builder.resource_subscriptions(uris);
        }
        if accepts_tasks {
            if let Some(task_ids) = &requested.task_ids {
                builder = builder.task_ids(task_ids.clone());
            }
        }
        Some(requested.intersection(&builder.build()))
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), McpError> {
        // 现代订阅由单一资源能力承接；多能力 fan-out 需要独立 sink，首阶段不做。
        for entry in self.ready_entries() {
            if entry.accepts_subscriptions() && entry.declared.name == "workspace" {
                return guarded(entry.declared.name, entry.handler.listen(context)).await;
            }
        }
        context.cancelled().await;
        Ok(())
    }

    async fn list_skills(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListSkillsResult, McpError> {
        let entry = self.require_ready("workspace")?;
        guarded(
            entry.declared.name,
            entry.handler.list_skills(request, context),
        )
        .await
    }

    async fn get_skill(
        &self,
        request: GetSkillRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<GetSkillResult, McpError> {
        let entry = self.require_ready("workspace")?;
        guarded(
            entry.declared.name,
            entry.handler.get_skill(request, context),
        )
        .await
    }

    async fn on_custom_request(
        &self,
        request: CustomRequest,
        context: RequestContext<RoleServer>,
    ) -> Result<CustomResult, McpError> {
        if let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.is_ready() && entry.declared.name == "workspace")
        {
            return guarded(
                entry.declared.name,
                entry.handler.on_custom_request(request, context),
            )
            .await;
        }
        Err(McpError::new(
            ErrorCode::METHOD_NOT_FOUND,
            "custom requests require the workspace capability",
            None,
        ))
    }

    async fn get_task(
        &self,
        request: GetTaskParams,
        context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, McpError> {
        let entry = self.route_task(&request.task_id, "tasks/get")?;
        guarded(
            entry.declared.name,
            entry.handler.get_task(request, context),
        )
        .await
    }

    async fn update_task(
        &self,
        request: UpdateTaskParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        let entry = self.route_task(&request.task_id, "tasks/update")?;
        guarded(
            entry.declared.name,
            entry.handler.update_task(request, context),
        )
        .await
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        let entry = self.route_task(&request.task_id, "tasks/cancel")?;
        guarded(
            entry.declared.name,
            entry.handler.cancel_task(request, context),
        )
        .await
    }
}

impl std::fmt::Debug for BuiltinRouter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BuiltinRouter")
            .field("effective", &self.effective_capabilities())
            .field("disabled", &self.disabled)
            .finish()
    }
}

#[path = "router/readiness.rs"]
mod readiness;
#[path = "router/router_handler.rs"]
mod router_handler;
#[path = "router/tasks.rs"]
mod tasks;

pub use readiness::{CapabilityReadiness, CapabilityStatus, RouterReadiness};
use tasks::TaskRoutes;

#[cfg(test)]
#[path = "router_test.rs"]
mod tests;

#[cfg(test)]
#[path = "router_test_support.rs"]
pub(crate) mod router_test_support;
