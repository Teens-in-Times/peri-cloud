use std::sync::Arc;

use peri_acp_types::tools::{BaseTool, ToolContext, ToolDefinition, ToolOutput};
use peri_tool_runtime::filesystem::{
    EditFileTool, GlobFilesTool, GrepTool, ReadFileTool, WriteFileTool,
};
use peri_tool_runtime::shell_contract::{bash_parameters, BASH_DESCRIPTION};
use tokio_util::sync::CancellationToken;

use crate::{Error, Result};
use peri_acp_types::device_executor::{NativeToolDescriptor, SessionBinding, SubmitJob};

pub(crate) struct NativeTools {
    pub binding: SessionBinding,
    tools: Vec<Arc<dyn BaseTool>>,
}

impl NativeTools {
    pub fn new(binding: SessionBinding) -> Self {
        let cwd = binding.workspace.clone();
        Self {
            binding,
            tools: vec![
                Arc::new(ReadFileTool::new(cwd.clone())),
                Arc::new(WriteFileTool::new(cwd.clone())),
                Arc::new(EditFileTool::new(cwd.clone())),
                Arc::new(GlobFilesTool::new(cwd.clone())),
                Arc::new(GrepTool::new(cwd)),
            ],
        }
    }

    pub fn descriptors(&self) -> Vec<NativeToolDescriptor> {
        let mut tools: Vec<_> = self
            .tools
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
            .collect();
        tools.push(NativeToolDescriptor {
            definition: ToolDefinition {
                name: "Bash".into(),
                description: BASH_DESCRIPTION.into(),
                parameters: bash_parameters(),
            },
            aliases: vec!["Shell".into()],
            namespace: Some("execution".into()),
            is_direct: true,
            output_char_limit: Some(10_000),
            prefers_persist: false,
            context_retention: peri_acp_types::tools::ContextRetention::Preserve,
            prompt_declaration: Some("Run a shell command → `{{name}}` ({{title}}).".into()),
        });
        tools
    }

    pub fn validate(&self, request: &SubmitJob) -> Result<()> {
        if is_shell(&request.tool) {
            let command = request.input.get("command").and_then(|v| v.as_str());
            if command.is_none_or(|s| s.is_empty()) {
                return Err(Error::Invalid("Bash requires a nonempty command".into()));
            }
            if let Some(timeout) = request.input.get("timeout") {
                if !timeout.is_null() && timeout.as_u64().is_none() {
                    return Err(Error::Invalid(
                        "timeout must be a nonnegative integer".into(),
                    ));
                }
            }
            if let Some(background) = request.input.get("run_in_background") {
                if !background.is_boolean() {
                    return Err(Error::Invalid("run_in_background must be boolean".into()));
                }
            }
            return Ok(());
        }
        self.resolve(&request.tool).map(|_| ())
    }

    fn resolve(&self, name: &str) -> Result<Arc<dyn BaseTool>> {
        self.tools
            .iter()
            .find(|tool| {
                tool.name().eq_ignore_ascii_case(name)
                    || tool
                        .aliases()
                        .iter()
                        .any(|alias| alias.eq_ignore_ascii_case(name))
            })
            .cloned()
            .ok_or_else(|| Error::Invalid("unknown native tool".into()))
    }

    pub async fn invoke_file(
        &self,
        request: SubmitJob,
        cancellation: CancellationToken,
    ) -> std::result::Result<ToolOutput, String> {
        let tool = self
            .resolve(&request.tool)
            .map_err(|error| error.to_string())?;
        let cwd = self.binding.workspace.clone();
        let runtime = tokio::runtime::Handle::current();
        // Upstream file tools contain blocking filesystem calls. Keep them off
        // the HTTP runtime; completion remains truthful if cancellation races
        // an atomic commit. We never abort this worker and claim rollback.
        tokio::task::spawn_blocking(move || {
            runtime.block_on(async {
                let mut context = ToolContext::new(&[], &cwd);
                context.invocation_id = Some(request.invocation_id.to_string());
                context.session_id = Some(request.session_id.to_string());
                context.cancellation = cancellation;
                tool.invoke_output(request.input, context)
                    .await
                    .map_err(|error| error.to_string())
            })
        })
        .await
        .map_err(|_| "file worker terminated unexpectedly".to_owned())?
    }
}

pub(crate) fn is_shell(name: &str) -> bool {
    name.eq_ignore_ascii_case("Bash") || name.eq_ignore_ascii_case("Shell")
}
