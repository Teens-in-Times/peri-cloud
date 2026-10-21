use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use peri_acp_types::goal::GoalController;
use peri_acp_types::interaction::UserInteractionBroker;
use peri_acp_types::permission::SharedPermissionMode;
use peri_agent::middleware::chain::MiddlewareChain;
use peri_agent::middleware::r#trait::Middleware;
use peri_agent::session::factory::{ChainSlot, MiddlewareChainAssembler};
use peri_agent::tools::BaseTool;
use peri_mcp_web::{web_fetch::WebFetchTool, web_search::WebSearchTool};
use peri_middlewares::hitl::HumanInTheLoopMiddleware;
use peri_middlewares::mcp::{McpClientPool, McpMiddleware};
use peri_middlewares::middleware::TodoMiddleware;
use peri_middlewares::permission::{
    default_requires_approval, AutoClassifier, PermissionMiddleware,
};
use peri_middlewares::tool_search::{ToolSearchIndex, ToolSearchMiddleware};
use peri_middlewares::tools::TodoItem;
use peri_middlewares::{AgentsMdMiddleware, GoalMiddleware};
use peri_remote_tools::RemoteSession;
use tokio::sync::mpsc;

use crate::workspace::{CloudSkillsMiddleware, DeviceWorkspace};

pub(crate) struct CloudAssemblyContext {
    pub remote: Option<Arc<RemoteSession>>,
    pub broker: Arc<dyn UserInteractionBroker>,
    pub permissions: Arc<SharedPermissionMode>,
    pub classifier: Option<Arc<dyn AutoClassifier>>,
    pub personal: Option<crate::personal::PersonalScope>,
    pub web: CloudWebTools,
    pub goal: Option<Arc<dyn GoalController>>,
    pub tool_search: Option<CloudToolSearch>,
    pub workspace: Option<Arc<DeviceWorkspace>>,
    pub mcp_pool: Option<Arc<McpClientPool>>,
    pub session_id: String,
}

/// Shared index and per-turn tool map for the deferred-tool meta tools. Both
/// point at the same map the turn later hands to `StageContext`.
pub(crate) struct CloudToolSearch {
    pub index: Arc<ToolSearchIndex>,
    pub tools: Arc<RwLock<BTreeMap<String, Arc<dyn BaseTool>>>>,
}

/// Cloud-side builtin tools. Web access is opt-in through the deployment's
/// service environment; the tool names are already handled by
/// `default_requires_approval`, so no second approval policy is introduced.
#[derive(Clone, Default)]
pub(crate) struct CloudWebTools {
    search: Option<HashMap<String, String>>,
    fetch: Option<HashMap<String, String>>,
}

impl CloudWebTools {
    pub(crate) fn from_env() -> Self {
        Self::from_instances(
            web_instance_env("PERI_WEB_SEARCH_URL", "PERI_WEB_SEARCH_TOKEN"),
            web_instance_env("PERI_WEB_FETCH_URL", "PERI_WEB_FETCH_TOKEN"),
        )
    }

    pub(crate) fn from_instances(
        search: Option<HashMap<String, String>>,
        fetch: Option<HashMap<String, String>>,
    ) -> Self {
        Self { search, fetch }
    }
}

/// A tool is enabled only when its endpoint is configured. The upstream
/// constructor falls back to a public backend when the URL is missing, so
/// enabling on a token alone would send that token to an endpoint the
/// deployment never named.
fn web_instance_env(url_key: &str, token_key: &str) -> Option<HashMap<String, String>> {
    let url = non_empty_env(url_key)?;
    let mut env = HashMap::from([(url_key.to_owned(), url)]);
    if let Some(token) = non_empty_env(token_key) {
        env.insert(token_key.to_owned(), token);
    }
    Some(env)
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Cloud additions to the shared tool approval policy. `mcp_read_resource`
/// reads external server resources and is not covered by the `mcp__` tool-name
/// prefix, so it is listed explicitly. Everything else keeps the Peri baseline.
fn requires_approval(name: &str) -> bool {
    matches!(
        name,
        "AddMemory"
            | "UpdateMemory"
            | "ForgetMemory"
            | "schedule_create"
            | "schedule_update"
            | "schedule_remove"
            | "mcp_read_resource"
    ) || default_requires_approval(name)
}

/// Only installed cloud capabilities are enabled. Slot order remains Peri-owned.
pub(crate) struct CloudChainAssembler;

impl MiddlewareChainAssembler for CloudChainAssembler {
    type Context = CloudAssemblyContext;
    type Output = MiddlewareChain;

    fn assemble(&self, blueprint: &[ChainSlot], ctx: &Self::Context) -> Self::Output {
        let mut chain = MiddlewareChain::new();
        // v4 removed the Filesystem/Terminal slots. The device tools are
        // installed explicitly before approval, preserving the old chain order.
        if let Some(remote) = &ctx.remote {
            chain.add(Box::new(DeviceTools {
                remote: remote.clone(),
                shell: false,
            }));
            chain.add(Box::new(DeviceTools {
                remote: remote.clone(),
                shell: true,
            }));
        }
        chain.add(Box::new(CloudBuiltinTools {
            web: ctx.web.clone(),
        }));
        if let Some(environment) = ctx.workspace.as_ref().and_then(|w| w.environment.clone()) {
            chain.add(Box::new(crate::workspace::EnvironmentContext(environment)));
        }
        for slot in blueprint {
            match slot {
                ChainSlot::Permission => {
                    chain.add(Box::new(PermissionMiddleware::with_shared_mode(
                        ctx.broker.clone(),
                        requires_approval,
                        ctx.permissions.clone(),
                        ctx.classifier.clone(),
                    )))
                }
                ChainSlot::AskUser => {
                    chain.add(Box::new(HumanInTheLoopMiddleware::new(ctx.broker.clone())))
                }
                ChainSlot::AgentsMd => {
                    if let Some(workspace) = &ctx.workspace {
                        if workspace.has_instructions() {
                            chain.add(Box::new(AgentsMdMiddleware::new().with_frozen_parts(
                                workspace.main.clone(),
                                workspace.local.clone(),
                            )));
                        }
                    }
                }
                ChainSlot::Skills => {
                    if let Some(workspace) = &ctx.workspace {
                        if workspace.skills_available {
                            chain.add(Box::new(CloudSkillsMiddleware::new(workspace.clone())));
                        }
                    }
                }
                ChainSlot::Todo => chain.add(Box::new(TodoMiddleware::new(todo_channel()))),
                ChainSlot::ToolSearch => {
                    if let Some(search) = &ctx.tool_search {
                        chain.add(Box::new(ToolSearchMiddleware::new(
                            search.index.clone(),
                            search.tools.clone(),
                        )));
                    }
                }
                ChainSlot::Goal => {
                    // The cloud model is the account model; reusing it as the
                    // completion verifier would add an unaccounted aux call in
                    // the tool path, so completion is unverified for now.
                    if let Some(goal) = &ctx.goal {
                        chain.add(Box::new(GoalMiddleware::new(goal.clone(), None)));
                    }
                }
                ChainSlot::Mcp => {
                    if let Some(pool) = &ctx.mcp_pool {
                        chain.add(Box::new(
                            McpMiddleware::new(pool.clone())
                                .with_session_id(ctx.session_id.clone()),
                        ));
                    }
                }
                // Hooks, plugins, subagents, workflows, image and at-mention
                // still assume a local host cwd or local resources. They stay
                // off until a remote-capable provider exists; skills above use
                // the authenticated workspace-read endpoint, never the cloud
                // host's filesystem.
                _ => {}
            }
        }
        if let Some(scope) = &ctx.personal {
            chain.add(Box::new(crate::scheduler::tools::ScheduleTools(
                scope.clone(),
            )));
            chain.add(Box::new(crate::memory::tools::MemoryTools(scope.clone())));
        }
        chain
    }
}

struct CloudBuiltinTools {
    web: CloudWebTools,
}

#[async_trait]
impl Middleware for CloudBuiltinTools {
    fn name(&self) -> &str {
        "CloudBuiltinTools"
    }
    fn collect_tools(&self, _cwd: &str) -> Vec<Box<dyn BaseTool>> {
        let mut tools: Vec<Box<dyn BaseTool>> = Vec::new();
        if let Some(env) = &self.web.search {
            tools.push(Box::new(WebSearchTool::from_instance_env(env)));
        }
        if let Some(env) = &self.web.fetch {
            tools.push(Box::new(WebFetchTool::from_instance_env(env)));
        }
        tools
    }
}

/// Todo notifications are a TUI surface. The cloud keeps the channel drained so
/// `TodoWrite` never blocks or logs a closed-channel warning; it does not expose
/// a second UI path.
fn todo_channel() -> mpsc::Sender<Vec<TodoItem>> {
    let (tx, mut rx) = mpsc::channel(32);
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move { while rx.recv().await.is_some() {} });
    } else {
        std::thread::spawn(move || while rx.blocking_recv().is_some() {});
    }
    tx
}

struct DeviceTools {
    remote: Arc<RemoteSession>,
    shell: bool,
}

#[async_trait]
impl Middleware for DeviceTools {
    fn name(&self) -> &str {
        if self.shell {
            "DeviceTerminalMiddleware"
        } else {
            "DeviceFilesystemMiddleware"
        }
    }
    fn collect_tools(&self, _cwd: &str) -> Vec<Box<dyn BaseTool>> {
        self.remote
            .tools()
            .into_iter()
            .filter(|tool| {
                let shell = matches!(tool.name(), "Bash" | "GetExecution" | "CancelExecution");
                shell == self.shell
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "assembly_test.rs"]
mod tests;
