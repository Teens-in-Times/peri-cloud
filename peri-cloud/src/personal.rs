//! Account identity is captured by deployment, never supplied by the model.
use crate::state::CloudJournal;
use async_trait::async_trait;
use peri_agent::middleware::r#trait::Middleware;
use peri_agent::tools::{BaseTool, ToolContext};
use peri_model::{
    Model, ModelCapabilities, ModelError, ModelRequest, ModelResult, ModelStream,
    PreparedModelRequest, ProtocolErrorKind,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone)]
pub(crate) struct PersonalScope {
    pub journal: Arc<CloudJournal>,
    pub principal: Uuid,
    pub max_bytes: usize,
}

pub(crate) struct PersonalTools(pub PersonalScope);
#[async_trait]
impl Middleware for PersonalTools {
    fn name(&self) -> &str {
        "PersonalMemoryMiddleware"
    }
    fn collect_tools(&self, _cwd: &str) -> Vec<Box<dyn BaseTool>> {
        vec![
            Box::new(MemoryTool {
                scope: self.0.clone(),
                write: false,
            }),
            Box::new(MemoryTool {
                scope: self.0.clone(),
                write: true,
            }),
        ]
    }
}

struct MemoryTool {
    scope: PersonalScope,
    write: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryUpdate {
    revision: u64,
    memory: String,
}
#[async_trait]
impl BaseTool for MemoryTool {
    fn name(&self) -> &str {
        if self.write {
            "WriteMemory"
        } else {
            "ReadMemory"
        }
    }
    fn is_direct(&self) -> bool {
        true
    }
    fn description(&self) -> &str {
        if self.write {
            "Replace this user's long-term memory. ReadMemory first; use its revision. Save only useful facts the user wants remembered, never credentials. Other users cannot be selected."
        } else {
            "Read this user's long-term memory and its revision. Other users cannot be selected."
        }
    }
    fn parameters(&self) -> Value {
        if self.write {
            json!({"type":"object","properties":{"revision":{"type":"integer","minimum":0},"memory":{"type":"string"}},"required":["revision","memory"],"additionalProperties":false})
        } else {
            json!({"type":"object","properties":{},"additionalProperties":false})
        }
    }
    async fn invoke(
        &self,
        input: Value,
        _ctx: ToolContext<'_>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let mut data = self.scope.journal.user_data(self.scope.principal).await?;
        if self.write {
            let update: MemoryUpdate = serde_json::from_value(input)?;
            if update.revision != data.revision {
                return Err(crate::state::StateError::InvocationConflict.into());
            }
            data.memory = update.memory;
            data = self
                .scope
                .journal
                .save_user_data(self.scope.principal, data, self.scope.max_bytes)
                .await?;
        } else if input.as_object().is_none_or(|obj| !obj.is_empty()) {
            return Err(crate::state::StateError::InvocationConflict.into());
        }
        Ok(serde_json::to_string(
            &json!({"revision":data.revision,"memory":data.memory}),
        )?)
    }
}

pub(crate) struct MeteredModel {
    pub inner: Arc<dyn Model>,
    pub journal: Arc<CloudJournal>,
    pub principal: Uuid,
    pub daily_limit: u32,
}
#[async_trait]
impl Model for MeteredModel {
    fn capabilities(&self) -> ModelCapabilities {
        self.inner.capabilities()
    }
    fn prepare_request(&self, request: &ModelRequest) -> ModelResult<PreparedModelRequest> {
        self.inner.prepare_request(request)
    }
    async fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> ModelResult<ModelStream> {
        if cancellation.is_cancelled() {
            return Err(ModelError::cancelled());
        }
        self.journal
            .charge_model_request(self.principal, self.daily_limit)
            .await
            .map_err(|_| {
                ModelError::protocol_with_summary(
                    ProtocolErrorKind::Other,
                    "account_model_request_limit",
                )
            })?;
        self.inner.stream(request, cancellation).await
    }
}
