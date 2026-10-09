use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use peri_acp_types::device_executor::NativeToolDescriptor;
use peri_acp_types::tools::{BaseTool, ContextRetention, ToolContext, ToolOutput};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::session::snapshot_output;
use crate::{Error, RemoteSession};

type ToolResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub(crate) struct RemoteTool {
    pub session: Arc<RemoteSession>,
    pub descriptor: NativeToolDescriptor,
}

#[async_trait]
impl BaseTool for RemoteTool {
    fn name(&self) -> &str {
        &self.descriptor.definition.name
    }
    fn description(&self) -> &str {
        &self.descriptor.definition.description
    }
    fn parameters(&self) -> Value {
        self.descriptor.definition.parameters.clone()
    }
    fn aliases(&self) -> &[&str] {
        match self.name() {
            "Read" => &["reading"],
            "Bash" => &["Shell"],
            _ => &[],
        }
    }
    fn timeout(&self) -> Option<Duration> {
        None
    }
    fn namespace(&self) -> Option<&str> {
        self.descriptor.namespace.as_deref()
    }
    fn is_direct(&self) -> bool {
        self.descriptor.is_direct
    }
    fn output_char_limit(&self) -> Option<usize> {
        self.descriptor.output_char_limit
    }
    fn prefers_persist(&self) -> bool {
        self.descriptor.prefers_persist
    }
    fn context_retention(&self) -> ContextRetention {
        self.descriptor.context_retention
    }
    fn prompt_declaration(&self) -> Option<String> {
        self.descriptor.prompt_declaration.clone()
    }

    async fn invoke(&self, input: Value, ctx: ToolContext<'_>) -> ToolResult<String> {
        Ok(self
            .invoke_output(input, ctx)
            .await?
            .projected_text(self.output_char_limit()))
    }

    async fn invoke_output(&self, input: Value, ctx: ToolContext<'_>) -> ToolResult<ToolOutput> {
        Ok(self.session.invoke(self.name(), input, ctx).await?)
    }
}

pub(crate) struct ControlTool {
    pub session: Arc<RemoteSession>,
    pub cancel: bool,
}

#[async_trait]
impl BaseTool for ControlTool {
    fn name(&self) -> &str {
        if self.cancel {
            "CancelExecution"
        } else {
            "GetExecution"
        }
    }
    fn description(&self) -> &str {
        if self.cancel {
            "Request cancellation of a task on this session's device. The request does not prove that the task has stopped; query its actual status."
        } else {
            "Query a previously admitted task on this session's device. Use its task_id to recover after a disconnect or wait deadline, without executing the operation again."
        }
    }
    fn parameters(&self) -> Value {
        json!({"type":"object", "properties":{"task_id":{"type":"string","format":"uuid"}},"required":["task_id"],"additionalProperties":false})
    }
    fn namespace(&self) -> Option<&str> {
        Some("execution")
    }
    fn timeout(&self) -> Option<Duration> {
        None
    }
    fn is_direct(&self) -> bool {
        true
    }
    fn context_retention(&self) -> ContextRetention {
        ContextRetention::StateBearing
    }

    async fn invoke(&self, input: Value, ctx: ToolContext<'_>) -> ToolResult<String> {
        Ok(self
            .invoke_output(input, ctx)
            .await?
            .projected_text(Some(10_000)))
    }

    async fn invoke_output(&self, input: Value, ctx: ToolContext<'_>) -> ToolResult<ToolOutput> {
        self.session.check_context(&ctx)?;
        let object = input.as_object().ok_or(Error::Protocol)?;
        if object.len() != 1 {
            return Err(Error::Protocol.into());
        }
        let id = object
            .get("task_id")
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or(Error::Protocol)?;
        let job = self.session.task(id, self.cancel).await?;
        Ok(snapshot_output(job))
    }
}
