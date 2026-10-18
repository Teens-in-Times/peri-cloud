use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use peri_acp_types::device_executor::WorkspaceFile;
use peri_acp_types::goal::{GoalController, GoalViewSnapshot};
use peri_acp_types::interaction::{
    InteractionContext, InteractionResponse, TrustedInteractionOwner, UserInteractionBroker,
};
use peri_acp_types::permission::{PermissionMode, SharedPermissionMode};
use peri_agent::session::factory::build_middleware_chain;
use peri_agent::tools::BaseTool;
use peri_middlewares::tool_search::ToolSearchIndex;

use super::{CloudAssemblyContext, CloudChainAssembler, CloudToolSearch, CloudWebTools};
use crate::workspace::{DeviceWorkspace, SkillReader, WorkspaceSkill};

struct RejectingBroker;

#[async_trait]
impl UserInteractionBroker for RejectingBroker {
    async fn request(
        &self,
        _owner: TrustedInteractionOwner,
        _context: InteractionContext,
    ) -> InteractionResponse {
        InteractionResponse::Rejected
    }
}

struct NoGoal;

#[async_trait]
impl GoalController for NoGoal {
    async fn create_goal(&self, _objective: String) -> Result<(), String> {
        Ok(())
    }
    async fn complete_goal(&self) -> Result<(), String> {
        Ok(())
    }
    async fn block_goal(&self, _reason: String) -> Result<(), String> {
        Ok(())
    }
    async fn clear_goal(&self) -> Result<(), String> {
        Ok(())
    }
    fn snapshot(&self) -> GoalViewSnapshot {
        GoalViewSnapshot::default()
    }
}

fn context(web: CloudWebTools) -> CloudAssemblyContext {
    CloudAssemblyContext {
        remote: None,
        broker: Arc::new(RejectingBroker),
        permissions: SharedPermissionMode::new(PermissionMode::Default),
        classifier: None,
        personal: None,
        web,
        goal: None,
        tool_search: None,
        workspace: None,
        mcp_pool: None,
        session_id: String::new(),
    }
}

fn tools(context: &CloudAssemblyContext) -> BTreeMap<String, Arc<dyn BaseTool>> {
    build_middleware_chain(&CloudChainAssembler, context)
        .collect_tools("")
        .into_iter()
        .map(|tool| (tool.name().to_owned(), Arc::from(tool)))
        .collect()
}

fn tool_names(context: &CloudAssemblyContext) -> Vec<String> {
    tools(context).into_keys().collect()
}

#[test]
fn todo_tool_is_always_available() {
    let names = tool_names(&context(CloudWebTools::default()));
    assert!(names.iter().any(|name| name == "TodoWrite"));
}

#[test]
fn web_tools_are_opt_in_and_keep_the_existing_approval_policy() {
    let names = tool_names(&context(CloudWebTools::default()));
    assert!(!names.iter().any(|name| name == "WebSearch"));
    assert!(!names.iter().any(|name| name == "WebFetch"));

    let names = tool_names(&context(CloudWebTools::from_instances(
        Some(HashMap::from([(
            "PERI_WEB_SEARCH_TOKEN".to_owned(),
            "test".to_owned(),
        )])),
        Some(HashMap::from([(
            "PERI_WEB_FETCH_TOKEN".to_owned(),
            "test".to_owned(),
        )])),
    )));
    assert!(names.iter().any(|name| name == "WebSearch"));
    assert!(names.iter().any(|name| name == "WebFetch"));

    assert!(super::default_requires_approval("WebSearch"));
    assert!(super::default_requires_approval("WebFetch"));
    assert!(!super::default_requires_approval("TodoWrite"));
}

#[test]
fn web_instance_env_requires_an_explicit_endpoint() {
    let url_key = "PERI_TEST_WEB_SEARCH_URL";
    let token_key = "PERI_TEST_WEB_SEARCH_TOKEN";
    std::env::remove_var(url_key);
    std::env::set_var(token_key, "secret");

    // A credential without an endpoint must not enable the public fallback.
    assert!(super::web_instance_env(url_key, token_key).is_none());

    std::env::set_var(url_key, "https://search.example");
    let env = super::web_instance_env(url_key, token_key).expect("endpoint enables the tool");
    assert_eq!(
        env.get(url_key).map(String::as_str),
        Some("https://search.example")
    );
    assert_eq!(env.get(token_key).map(String::as_str), Some("secret"));

    std::env::remove_var(url_key);
    std::env::remove_var(token_key);
}

#[test]
fn goal_is_a_deferred_tool_reachable_through_tool_search() {
    let shared = Arc::new(RwLock::new(BTreeMap::<String, Arc<dyn BaseTool>>::new()));
    let mut context = context(CloudWebTools::default());
    context.goal = Some(Arc::new(NoGoal));
    context.tool_search = Some(CloudToolSearch {
        index: Arc::new(ToolSearchIndex::new()),
        tools: shared,
    });

    let names = tool_names(&context);
    assert!(names.iter().any(|name| name == "goal"));
    assert!(names.iter().any(|name| name == "SearchExtraTools"));
    assert!(names.iter().any(|name| name == "ExecuteExtraTool"));

    let map = tools(&context);
    // Deferred tools are intentionally absent from the direct model catalog.
    assert!(!map["goal"].is_direct());
    assert!(map["goal"].visible_to_model());
}

#[test]
fn goal_and_tool_search_are_off_without_an_account_scope() {
    let names = tool_names(&context(CloudWebTools::default()));
    assert!(!names.iter().any(|name| name == "goal"));
    assert!(!names.iter().any(|name| name == "SearchExtraTools"));
    assert!(!names.iter().any(|name| name == "ExecuteExtraTool"));
}

#[test]
fn cloud_approval_policy_covers_mcp_and_memory_writes() {
    assert!(super::requires_approval("mcp_read_resource"));
    assert!(super::requires_approval("mcp__docs__search"));
    assert!(super::requires_approval("AddMemory"));
    assert!(super::requires_approval("Write"));
    assert!(!super::requires_approval("SkillTool"));
    assert!(!super::requires_approval("TodoWrite"));
    assert!(!super::requires_approval("DiscoverMCP"));
}

struct UnusedReader;

#[async_trait]
impl SkillReader for UnusedReader {
    async fn read(&self, _path: &str) -> Result<WorkspaceFile, String> {
        Err("not configured".into())
    }
}

fn workspace(
    main: Option<&str>,
    skills: Vec<WorkspaceSkill>,
    skills_available: bool,
) -> Arc<DeviceWorkspace> {
    Arc::new(DeviceWorkspace {
        main: main.map(str::to_owned),
        local: None,
        skills,
        skills_available,
        reader: Arc::new(UnusedReader),
    })
}

fn contributions(context: &CloudAssemblyContext) -> String {
    build_middleware_chain(&CloudChainAssembler, context)
        .collect_prompt_contributions()
        .unwrap()
}

#[test]
fn project_instructions_are_injected_only_from_the_device_workspace() {
    let mut context = context(CloudWebTools::default());
    assert!(!contributions(&context).contains("project rules"));

    context.workspace = Some(workspace(Some("project rules"), Vec::new(), false));
    assert!(contributions(&context).contains("project rules"));
}

#[test]
fn available_skills_install_the_tools_and_catalog() {
    let mut context = context(CloudWebTools::default());
    context.workspace = Some(workspace(
        None,
        vec![WorkspaceSkill {
            name: "review".into(),
            path: ".claude/skills/review/SKILL.md".into(),
        }],
        true,
    ));

    let names = tool_names(&context);
    assert!(names.iter().any(|name| name == "SkillTool"));
    assert!(names.iter().any(|name| name == "DiscoverSkillsTool"));
    assert!(contributions(&context).contains("**review**"));
    assert!(tools(&context)["SkillTool"].is_direct());
}

#[test]
fn failed_skill_discovery_installs_nothing() {
    let mut context = context(CloudWebTools::default());
    context.workspace = Some(workspace(
        None,
        vec![WorkspaceSkill {
            name: "review".into(),
            path: ".claude/skills/review/SKILL.md".into(),
        }],
        false,
    ));

    let names = tool_names(&context);
    assert!(!names.iter().any(|name| name == "SkillTool"));
    assert!(!names.iter().any(|name| name == "DiscoverSkillsTool"));
    assert!(!contributions(&context).contains("**review**"));
}

#[test]
fn an_account_mcp_pool_installs_the_mcp_surface() {
    let mut with_mcp = context(CloudWebTools::default());
    with_mcp.mcp_pool = Some(Arc::new(peri_middlewares::mcp::McpClientPool::new_pending()));
    with_mcp.session_id = "session".into();

    let names = tool_names(&with_mcp);
    assert!(names.iter().any(|name| name == "mcp_read_resource"));
    assert!(names.iter().any(|name| name == "DiscoverMCP"));

    let without = context(CloudWebTools::default());
    let names = tool_names(&without);
    assert!(!names.iter().any(|name| name == "mcp_read_resource"));
    assert!(!names.iter().any(|name| name == "DiscoverMCP"));
}
