//! Main-only explicit routing. Native arguments stay typed; targets freeze before policy.
use super::devices::{projection, BoundDeviceTool, Prepare, TARGET};
use super::*;
use async_trait::async_trait;
use parking_lot::Mutex;
use peri_acp_types::device_executor::NativeToolDescriptor;
use peri_acp_types::tools::{BoundToolInvocation, ContextRetention};
use peri_agent::{
    agent::react::ToolCall,
    error::{AgentError, AgentResult},
    middleware::{capabilities::BeforeToolState, r#trait::Middleware},
    tools::{BaseTool, ToolContext, ToolOutput},
};
use peri_remote_tools::RemoteSession;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
};

const REQUEST: &str = "_device_request";
type ToolResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone)]
pub(crate) struct MainDeviceRouting {
    scope: TaskScope,
    turn: Uuid,
    remote: Option<Arc<RemoteSession>>,
    scheduled: bool,
    pending: Arc<Mutex<HashMap<Uuid, Arc<PendingCall>>>>,
}

impl MainDeviceRouting {
    pub async fn available(scope: &TaskScope) -> Result<bool> {
        let service = scope.service.upgrade().ok_or(TaskError::Unavailable)?;
        let devices =
            crate::memory::scope::devices(&service.journal.identity_pool(), scope.principal)
                .await
                .map_err(|_| TaskError::Unavailable)?;
        Ok(devices.iter().any(|d| !d.revoked))
    }

    pub fn new(
        scope: TaskScope,
        turn: Uuid,
        remote: Option<Arc<RemoteSession>>,
        scheduled: bool,
    ) -> Self {
        Self {
            scope,
            turn,
            remote,
            scheduled,
            pending: Default::default(),
        }
    }

    async fn freeze(&self, call: &ToolCall) -> Result<ToolCall> {
        let id = call.input[REQUEST]
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or(TaskError::Invalid)?;
        let pending = self
            .pending
            .lock()
            .remove(&id)
            .ok_or(TaskError::Forbidden)?;
        if pending.original != call.input || pending.spec.definition.name != call.name {
            return Err(TaskError::Forbidden);
        }
        let service = self.scope.service.upgrade().ok_or(TaskError::Unavailable)?;
        if self.scheduled
            && self
                .remote
                .as_ref()
                .is_none_or(|r| r.device_id() != pending.route.device_id)
        {
            return Err(TaskError::Forbidden);
        }
        let target = if is_control(&call.name) {
            service
                .control_target(&self.scope, self.turn, &pending.route, &pending.input)
                .await?
        } else if self.scheduled {
            // A scheduled device assignment keeps its original frozen environment.
            let remote = self.remote.as_ref().ok_or(TaskError::Forbidden)?;
            if pending.route.device_id != remote.device_id()
                || pending
                    .route
                    .workspace
                    .as_ref()
                    .is_some_and(|w| w != &remote.binding().workspace)
            {
                return Err(TaskError::Forbidden);
            }
            let frozen = service
                .journal
                .session(self.scope.principal, self.scope.session)
                .await
                .map_err(|_| TaskError::Forbidden)?
                .frozen;
            service
                .register_target(
                    &self.scope,
                    self.turn,
                    pending.route.workspace.clone(),
                    frozen,
                    remote.clone(),
                    None,
                )
                .await?
        } else {
            service
                .native_target(
                    &self.scope,
                    self.turn,
                    Prepare {
                        device_id: pending.route.device_id,
                        workspace: pending.route.workspace.clone(),
                    },
                )
                .await?
        };
        let inner = target
            .remote
            .tools()
            .into_iter()
            .find(|t| t.name() == call.name)
            .ok_or(TaskError::Invalid)?;
        let mut policy = pending.input.clone();
        policy[TARGET] = projection(&target.record);
        let bound = BoundDeviceTool {
            scope: self.scope.clone(),
            target,
            inner: Arc::from(inner),
            input: pending.input.clone(),
            policy: policy.clone(),
        };
        pending
            .bound
            .set(Arc::new(bound))
            .map_err(|_| TaskError::Conflict)?;
        Ok(ToolCall::new(call.id.clone(), call.name.clone(), policy))
    }
}

#[async_trait]
impl Middleware for MainDeviceRouting {
    fn name(&self) -> &str {
        "MainDeviceRouting"
    }

    fn collect_tools(&self, _: &str) -> Vec<Box<dyn BaseTool>> {
        let specs: Vec<NativeToolDescriptor> = match &self.remote {
            Some(remote) => remote
                .tools()
                .iter()
                .map(|tool| NativeToolDescriptor {
                    definition: tool.definition(),
                    aliases: tool.aliases().iter().map(|s| (*s).into()).collect(),
                    namespace: tool.namespace().map(str::to_owned),
                    is_direct: tool.is_direct(),
                    output_char_limit: tool.output_char_limit(),
                    prefers_persist: tool.prefers_persist(),
                    context_retention: tool.context_retention(),
                    prompt_declaration: tool.prompt_declaration(),
                })
                .collect(),
            None => peri_executor::native_tool_catalog()
                .into_iter()
                .chain(peri_remote_tools::control_tool_catalog())
                .collect(),
        };
        specs.into_iter().map(|spec| Box::new(MainDeviceTool {
            description: format!("{} Target device_id is required. Optional workspace selects its execution root; omission pins the device default for this turn. No preparation call is required.", spec.definition.description),
            spec, routing: self.clone(),
        }) as Box<dyn BaseTool>).collect()
    }

    async fn before_tool(
        &self,
        _: &mut dyn BeforeToolState,
        call: &ToolCall,
    ) -> AgentResult<ToolCall> {
        if call.input.get(REQUEST).is_none() {
            return Ok(call.clone());
        }
        self.freeze(call)
            .await
            .map_err(|error| AgentError::ToolRejected {
                tool: call.name.clone(),
                reason: format!("Device invocation was not dispatched: {error}"),
                cause: peri_acp_types::error::ToolRejectionCause::PolicyDenied,
            })
    }
}

pub(super) fn is_control(name: &str) -> bool {
    matches!(name, "GetExecution" | "CancelExecution")
}

struct MainDeviceTool {
    spec: NativeToolDescriptor,
    description: String,
    routing: MainDeviceRouting,
}

pub(super) fn parameters(native: Value) -> Value {
    let mut properties = native["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    properties.insert("device_id".into(), json!({"type":"string","format":"uuid","description":"Authorized target device ID from list_devices."}));
    properties.insert("workspace".into(), json!({"type":"string","minLength":1,"maxLength":4096,"description":"Target workspace. Omit to pin this device's default workspace for the turn."}));
    let mut required = native["required"].as_array().cloned().unwrap_or_default();
    required.retain(|v| v != "device_id");
    required.push(json!("device_id"));
    let mut schema = native.as_object().cloned().unwrap_or_default();
    schema.insert("type".into(), json!("object"));
    schema.insert("properties".into(), Value::Object(properties));
    schema.insert("required".into(), Value::Array(required));
    schema.insert("additionalProperties".into(), json!(false));
    Value::Object(schema)
}

#[async_trait]
impl BaseTool for MainDeviceTool {
    fn name(&self) -> &str {
        &self.spec.definition.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> Value {
        parameters(self.spec.definition.parameters.clone())
    }
    fn aliases(&self) -> &[&str] {
        match self.name() {
            "Read" => &["reading"],
            "Bash" => &["Shell"],
            _ => &[],
        }
    }
    fn is_direct(&self) -> bool {
        true
    }
    fn namespace(&self) -> Option<&str> {
        self.spec.namespace.as_deref()
    }
    fn timeout(&self) -> Option<std::time::Duration> {
        None
    }
    fn output_char_limit(&self) -> Option<usize> {
        self.spec.output_char_limit
    }
    fn prefers_persist(&self) -> bool {
        self.spec.prefers_persist
    }
    fn context_retention(&self) -> ContextRetention {
        self.spec.context_retention
    }
    fn prompt_declaration(&self) -> Option<String> {
        self.spec.prompt_declaration.clone()
    }

    fn bind_invocation(&self, input: Value) -> ToolResult<Option<BoundToolInvocation>> {
        let (route, native) = split_arguments(&input, &self.spec.definition.parameters)?;
        let mut original = input;
        let id = Uuid::new_v4();
        original[REQUEST] = json!(id);
        let pending = Arc::new(PendingCall {
            spec: self.spec.clone(),
            route,
            input: native,
            original: original.clone(),
            bound: OnceLock::new(),
        });
        let mut entries = self.routing.pending.lock();
        if entries.len() >= 128 {
            return Err(TaskError::Limit.into());
        }
        entries.insert(id, pending.clone());
        Ok(Some(BoundToolInvocation {
            policy_name: self.name().into(),
            policy_input: original,
            target: pending,
        }))
    }

    async fn invoke(&self, _: Value, _: ToolContext<'_>) -> ToolResult<String> {
        Err(
            "device invocation must pass through canonical routing and permission middleware"
                .into(),
        )
    }
}

struct PendingCall {
    spec: NativeToolDescriptor,
    route: Prepare,
    input: Value,
    original: Value,
    bound: OnceLock<Arc<BoundDeviceTool>>,
}

#[async_trait]
impl BaseTool for PendingCall {
    fn name(&self) -> &str {
        &self.spec.definition.name
    }
    fn description(&self) -> &str {
        &self.spec.definition.description
    }
    fn parameters(&self) -> Value {
        parameters(self.spec.definition.parameters.clone())
    }
    fn is_direct(&self) -> bool {
        true
    }
    fn timeout(&self) -> Option<std::time::Duration> {
        None
    }
    fn output_char_limit(&self) -> Option<usize> {
        self.spec.output_char_limit
    }
    fn context_retention(&self) -> ContextRetention {
        self.spec.context_retention
    }
    fn namespace(&self) -> Option<&str> {
        self.spec.namespace.as_deref()
    }
    fn prefers_persist(&self) -> bool {
        self.spec.prefers_persist
    }
    async fn invoke(&self, input: Value, ctx: ToolContext<'_>) -> ToolResult<String> {
        Ok(self
            .invoke_output(input, ctx)
            .await?
            .projected_text(self.output_char_limit()))
    }
    async fn invoke_output(&self, input: Value, ctx: ToolContext<'_>) -> ToolResult<ToolOutput> {
        self.bound
            .get()
            .ok_or(TaskError::Forbidden)?
            .invoke_output(input, ctx)
            .await
    }
}

pub(super) fn split_arguments(input: &Value, schema: &Value) -> ToolResult<(Prepare, Value)> {
    let mut native = input
        .as_object()
        .cloned()
        .ok_or("device tool arguments must be an object")?;
    let device_id = native
        .remove("device_id")
        .and_then(|v| v.as_str().and_then(|s| Uuid::parse_str(s).ok()))
        .ok_or("device_id is required and must be an authorized device UUID; use list_devices")?;
    let workspace = native
        .remove("workspace")
        .map(|v| {
            v.as_str()
                .filter(|s| !s.trim().is_empty() && s.len() <= 4096)
                .map(str::to_owned)
                .ok_or("workspace must be a nonempty path of at most 4096 bytes")
        })
        .transpose()?;
    let properties = &schema["properties"];
    if native
        .keys()
        .any(|key| key == TARGET || key == REQUEST || properties.get(key).is_none())
    {
        return Err("unknown native parameter or host-reserved routing field".into());
    }
    Ok((
        Prepare {
            device_id,
            workspace,
        },
        Value::Object(native),
    ))
}
