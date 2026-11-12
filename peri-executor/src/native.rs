use std::path::{Path, PathBuf};
use std::sync::Arc;

use peri_acp_types::tools::{BaseTool, ToolContext, ToolDefinition, ToolOutput};
use peri_tool_runtime::filesystem::{
    EditFileTool, GlobFilesTool, GrepTool, ReadFileTool, WriteFileTool,
};
use peri_tool_runtime::shell_contract::{bash_parameters, BASH_DESCRIPTION};
use tokio_util::sync::CancellationToken;

use crate::store::Store;
use crate::{Error, Result};
use peri_acp_types::device_executor::{NativeToolDescriptor, SessionBinding, SubmitJob};

/// 执行器私有路径拒绝列表（executor review 2026-10-26 P1-3，参照 reach 的
/// protected_paths）：transport-token（明文）、device.json、jobs.sqlite、审计与
/// 日志都在 state_dir 下，而 Read/Grep/Glob/folder 接受任意绝对路径——一条
/// `Read(state_dir/transport-token)` 就能把运输令牌读进上下文。
///
/// 这是纵深防御而非沙箱：Bash 仍然全能（文档如实声明），本列表让静默外泄在
/// 文件工具层必须显式失败。ancestor 语义：搜索根覆盖受保护路径（如整个盘符）
/// 同样拒绝，防止 Grep/Glob 以目录为单位绕过。
pub(crate) struct ProtectedPaths {
    roots: Vec<PathBuf>,
    logs: PathBuf,
}

impl ProtectedPaths {
    pub fn new(state_root: &Path) -> Self {
        let mut roots = Vec::new();
        if let Ok(canonical) = state_root.canonicalize() {
            roots.push(canonical);
        } else {
            roots.push(state_root.to_path_buf());
        }
        if let Ok(exe) = std::env::current_exe().and_then(|exe| exe.canonicalize()) {
            roots.push(exe);
        }
        let logs = roots[0].join("logs");
        Self { roots, logs }
    }

    /// `candidate` 落在任一受保护根内，或覆盖任一受保护根（作为祖先目录）。
    fn blocks(&self, candidate: &Path) -> bool {
        self.roots
            .iter()
            .any(|root| candidate.starts_with(root) || root.starts_with(candidate))
    }

    fn protected_target(
        &self,
        tool: &dyn BaseTool,
        input: &serde_json::Value,
        workspace: &str,
    ) -> Option<PathBuf> {
        let raw = match tool.name() {
            "Grep" | "Glob" => input["path"]
                .as_str()
                .filter(|p| !p.is_empty())
                .unwrap_or("."),
            _ => input["file_path"].as_str()?,
        };
        let resolved = peri_tool_runtime::filesystem::resolve_path(workspace, raw);
        let key = peri_tool_runtime::filesystem::transaction::target_key(workspace, raw);
        (self.blocks(&resolved) || self.blocks(&key)).then_some(resolved)
    }

    fn log_invocation(&self, resolved: &Path) -> Option<uuid::Uuid> {
        let parent = resolved.parent()?;
        if parent.parent()? != self.logs
            || !matches!(resolved.file_name()?.to_str()?, "stdout.log" | "stderr.log")
        {
            return None;
        }
        uuid::Uuid::parse_str(parent.file_name()?.to_str()?).ok()
    }
}

pub(crate) struct NativeTools {
    pub binding: SessionBinding,
    protected: ProtectedPaths,
    tools: Vec<Arc<dyn BaseTool>>,
}

impl NativeTools {
    pub fn new(binding: SessionBinding, state_root: &Path) -> Self {
        let cwd = binding.workspace.clone();
        Self {
            protected: ProtectedPaths::new(state_root),
            binding,
            tools: file_tools(cwd),
        }
    }

    pub fn descriptors(&self) -> Vec<NativeToolDescriptor> {
        descriptors(&self.tools)
    }

    pub async fn validate(&self, request: &SubmitJob, store: &Store) -> Result<()> {
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
        let tool = self.resolve(&request.tool)?;
        self.validate_file(tool.as_ref(), request, store).await
    }

    async fn validate_file(
        &self,
        tool: &dyn BaseTool,
        request: &SubmitJob,
        store: &Store,
    ) -> Result<()> {
        let Some(path) =
            self.protected
                .protected_target(tool, &request.input, &self.binding.workspace)
        else {
            return Ok(());
        };
        // The receipt's direct Read link is usable only in the original session.
        // The path check above uses the resolved target and excludes other state files.
        if tool.name() == "Read" {
            if let Some(id) = self.protected.log_invocation(&path) {
                let job = store.job(id).await.map_err(|error| match error {
                    Error::NotFound => Error::ProtectedPath,
                    other => other,
                })?;
                if job.session_id == self.binding.session_id
                    && [&job.stdout_path, &job.stderr_path]
                        .into_iter()
                        .flatten()
                        .any(|log| {
                            peri_tool_runtime::filesystem::resolve_path(
                                &self.binding.workspace,
                                log,
                            ) == path
                        })
                {
                    return Ok(());
                }
            }
        }
        Err(Error::ProtectedPath)
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
        store: &Store,
        cancellation: CancellationToken,
    ) -> std::result::Result<ToolOutput, String> {
        let tool = self
            .resolve(&request.tool)
            .map_err(|error| error.to_string())?;
        self.validate_file(tool.as_ref(), &request, store)
            .await
            .map_err(|error| error.to_string())?;
        let cwd = self.binding.workspace.clone();
        let runtime = tokio::runtime::Handle::current();
        // Upstream file tools contain blocking filesystem calls. Keep them off
        // the HTTP runtime; completion remains truthful if cancellation races
        // an atomic commit. We never abort this worker and claim rollback.
        tokio::task::spawn_blocking(move || {
            runtime.block_on(async {
                let mut context = ToolContext::new(&[], &cwd);
                context.invocation_id = Some(peri_acp_types::tool_invocation::InvocationId::new(
                    request.invocation_id.to_string(),
                ));
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

fn file_tools(cwd: String) -> Vec<Arc<dyn BaseTool>> {
    vec![
        Arc::new(ReadFileTool::new(cwd.clone())),
        Arc::new(WriteFileTool::new(cwd.clone())),
        Arc::new(EditFileTool::new(cwd.clone())),
        Arc::new(GlobFilesTool::new(cwd.clone())),
        Arc::new(GrepTool::new(cwd)),
    ]
}

/// Protocol schemas only; constructing the catalog performs no filesystem or shell operations.
pub fn native_tool_catalog() -> Vec<NativeToolDescriptor> {
    descriptors(&file_tools(String::new()))
}

fn descriptors(files: &[Arc<dyn BaseTool>]) -> Vec<NativeToolDescriptor> {
    let mut tools: Vec<_> = files
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
        output_char_limit: Some(peri_tool_runtime::budget::BASH_OUTPUT_CHAR_LIMIT),
        prefers_persist: false,
        context_retention: peri_acp_types::tools::ContextRetention::Preserve,
        prompt_declaration: Some("Run a shell command → `{{name}}` ({{title}}).".into()),
    });
    tools
}

#[cfg(test)]
mod protected_tests {
    use super::*;

    #[tokio::test]
    async fn file_tools_reject_state_tree_and_implicit_search_root() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let binding = SessionBinding {
            session_id: uuid::Uuid::new_v4(),
            workspace: dir
                .path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            workspace_identity: peri_acp_types::device_executor::WorkspaceIdentity {
                device: 0,
                inode: 0,
            },
        };
        let store = Store::open(&state).await.unwrap();
        let tools = NativeTools::new(binding.clone(), &state);
        let request = |tool: &str, input| SubmitJob {
            invocation_id: uuid::Uuid::new_v4(),
            session_id: binding.session_id,
            tool: tool.into(),
            input,
        };
        for invalid in [
            request(
                "reading",
                serde_json::json!({"file_path":"state/transport-token"}),
            ),
            request(
                "Write",
                serde_json::json!({"file_path":"state/new/nested.txt","content":"x"}),
            ),
            request("Grep", serde_json::json!({"pattern":"needle"})),
            request("Glob", serde_json::json!({"path":".","pattern":"**/*"})),
        ] {
            assert!(matches!(
                tools.validate(&invalid, &store).await,
                Err(Error::ProtectedPath)
            ));
        }
        assert!(tools
            .validate(
                &request("Read", serde_json::json!({"file_path":"notes.txt"})),
                &store,
            )
            .await
            .is_ok());
    }
}
