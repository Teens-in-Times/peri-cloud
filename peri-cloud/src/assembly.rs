use std::sync::Arc;

use async_trait::async_trait;
use peri_acp_types::interaction::UserInteractionBroker;
use peri_acp_types::permission::SharedPermissionMode;
use peri_agent::middleware::chain::MiddlewareChain;
use peri_agent::middleware::r#trait::Middleware;
use peri_agent::session::factory::{ChainSlot, MiddlewareChainAssembler};
use peri_agent::tools::BaseTool;
use peri_middlewares::hitl::HumanInTheLoopMiddleware;
use peri_middlewares::permission::{
    default_requires_approval, AutoClassifier, PermissionMiddleware,
};
use peri_remote_tools::RemoteSession;

pub(crate) struct CloudAssemblyContext {
    pub remote: Arc<RemoteSession>,
    pub broker: Arc<dyn UserInteractionBroker>,
    pub permissions: Arc<SharedPermissionMode>,
    pub classifier: Option<Arc<dyn AutoClassifier>>,
    pub personal: Option<crate::personal::PersonalScope>,
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
        chain.add(Box::new(DeviceTools {
            remote: ctx.remote.clone(),
            shell: false,
        }));
        chain.add(Box::new(DeviceTools {
            remote: ctx.remote.clone(),
            shell: true,
        }));
        for slot in blueprint {
            match slot {
                ChainSlot::Permission => {
                    chain.add(Box::new(PermissionMiddleware::with_shared_mode(
                        ctx.broker.clone(),
                        |name| name == "WriteMemory" || default_requires_approval(name),
                        ctx.permissions.clone(),
                        ctx.classifier.clone(),
                    )))
                }
                ChainSlot::AskUser => {
                    chain.add(Box::new(HumanInTheLoopMiddleware::new(ctx.broker.clone())))
                }
                // Local skills/hooks/git/PTC cannot read a remote cwd. They are
                // reserved for explicit remote-capable providers, not silently
                // enabled against the cloud server's filesystem.
                _ => {}
            }
        }
        if let Some(scope) = &ctx.personal {
            chain.add(Box::new(crate::personal::PersonalTools(scope.clone())));
        }
        chain
    }
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
