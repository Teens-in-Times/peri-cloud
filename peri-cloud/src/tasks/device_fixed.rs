//! Explicit addressing for embedded CloudAgent callers without a Gateway task service.
use super::device_routing::{parameters, split_arguments};
use async_trait::async_trait;
use peri_acp_types::tools::{BoundToolInvocation, ContextRetention};
use peri_agent::tools::{BaseTool, ToolContext, ToolOutput};
use peri_remote_tools::RemoteSession;
use serde_json::Value;
use std::sync::Arc;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub(crate) fn tools(remote: &Arc<RemoteSession>) -> Vec<Box<dyn BaseTool>> {
    remote
        .tools()
        .into_iter()
        .map(|inner| {
            Box::new(FixedTool {
                inner: Arc::from(inner),
                remote: remote.clone(),
            }) as Box<dyn BaseTool>
        })
        .collect()
}

struct FixedTool {
    inner: Arc<dyn BaseTool>,
    remote: Arc<RemoteSession>,
}

#[async_trait]
impl BaseTool for FixedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters(&self) -> Value {
        parameters(self.inner.parameters())
    }
    fn aliases(&self) -> &[&str] {
        self.inner.aliases()
    }
    fn namespace(&self) -> Option<&str> {
        self.inner.namespace()
    }
    fn is_direct(&self) -> bool {
        self.inner.is_direct()
    }
    fn timeout(&self) -> Option<std::time::Duration> {
        self.inner.timeout()
    }
    fn output_char_limit(&self) -> Option<usize> {
        self.inner.output_char_limit()
    }
    fn context_retention(&self) -> ContextRetention {
        self.inner.context_retention()
    }
    fn prompt_declaration(&self) -> Option<String> {
        self.inner.prompt_declaration()
    }
    fn bind_invocation(&self, input: Value) -> Result<Option<BoundToolInvocation>> {
        let (route, native) = split_arguments(&input, &self.inner.parameters())?;
        if route.device_id != self.remote.device_id()
            || route
                .workspace
                .as_ref()
                .is_some_and(|w| w != &self.remote.binding().workspace)
        {
            return Err("target is outside this embedded agent's frozen device/workspace".into());
        }
        Ok(Some(BoundToolInvocation {
            policy_name: self.name().into(),
            policy_input: input.clone(),
            target: Arc::new(FrozenTool {
                inner: self.inner.clone(),
                policy: input,
                native,
            }),
        }))
    }
    async fn invoke(&self, _: Value, _: ToolContext<'_>) -> Result<String> {
        Err("explicit device invocation requires canonical permission dispatch".into())
    }
}

struct FrozenTool {
    inner: Arc<dyn BaseTool>,
    policy: Value,
    native: Value,
}
#[async_trait]
impl BaseTool for FrozenTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters(&self) -> Value {
        parameters(self.inner.parameters())
    }
    fn is_direct(&self) -> bool {
        true
    }
    fn timeout(&self) -> Option<std::time::Duration> {
        self.inner.timeout()
    }
    fn output_char_limit(&self) -> Option<usize> {
        self.inner.output_char_limit()
    }
    fn namespace(&self) -> Option<&str> {
        self.inner.namespace()
    }
    fn prefers_persist(&self) -> bool {
        self.inner.prefers_persist()
    }
    fn context_retention(&self) -> ContextRetention {
        self.inner.context_retention()
    }
    async fn invoke(&self, input: Value, ctx: ToolContext<'_>) -> Result<String> {
        Ok(self
            .invoke_output(input, ctx)
            .await?
            .projected_text(self.output_char_limit()))
    }
    async fn invoke_output(&self, input: Value, ctx: ToolContext<'_>) -> Result<ToolOutput> {
        if input != self.policy {
            return Err("approved device invocation was modified".into());
        }
        self.inner.invoke_output(self.native.clone(), ctx).await
    }
}
