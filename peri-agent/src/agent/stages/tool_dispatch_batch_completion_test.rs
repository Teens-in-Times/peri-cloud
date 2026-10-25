//! 批量工具的完成事件与原子提交顺序。
use super::*;

#[tokio::test]
async fn test_dispatch_emits_fast_completion_before_atomic_batch_commit() {
    struct GatedTool {
        name: &'static str,
        release: Option<Arc<tokio::sync::Notify>>,
    }
    #[async_trait::async_trait]
    impl BaseTool for GatedTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "batch ordering fixture"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({})
        }
        async fn invoke(
            &self,
            _input: serde_json::Value,
            _ctx: crate::tools::ToolContext<'_>,
        ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
            if let Some(release) = &self.release {
                release.notified().await;
            }
            Ok(self.name.into())
        }
    }
    type HookTrace = Arc<parking_lot::Mutex<Vec<(&'static str, usize)>>>;
    struct Hooks(HookTrace);
    #[async_trait::async_trait]
    impl Middleware for Hooks {
        fn name(&self) -> &str {
            "BatchHooks"
        }
        async fn after_tool(
            &self,
            state: &mut dyn hook_state::AfterToolState,
            _call: &ToolCall,
            _result: &ToolResult,
        ) -> AgentResult<()> {
            self.0.lock().push(("after_tool", state.messages().len()));
            Ok(())
        }
        async fn after_tools_batch(
            &self,
            state: &mut dyn hook_state::AfterToolsBatchState,
            _results: &[(ToolCall, ToolResult)],
        ) -> AgentResult<()> {
            self.0.lock().push(("after_batch", state.messages().len()));
            Ok(())
        }
    }
    let (bus, mut handles) = crate::agent::events_v2::EventBus::new(Default::default());
    let turn = TurnContext::new(Arc::from("/tmp"), Arc::new(CancellationToken::new()));
    let transcript = Arc::new(parking_lot::RwLock::new(MessageTranscript::new()));
    transcript
        .write()
        .append(BaseMessage::human("previous history"));
    let mut ctx = StageContext::builder(turn, Arc::clone(&transcript), MessageQueue::new())
        .with_event_bus(Arc::new(bus))
        .build();
    let release = Arc::new(tokio::sync::Notify::new());
    for (name, gate) in [("Slow", Some(Arc::clone(&release))), ("Fast", None)] {
        ctx.runtime.tools.write().insert(
            name.into(),
            Arc::new(GatedTool {
                name,
                release: gate,
            }),
        );
    }
    let trace: HookTrace = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let mut chain = MiddlewareChain::new();
    chain.add(Box::new(Hooks(Arc::clone(&trace))));
    ctx.runtime.middleware_chain = Arc::new(chain);
    let reasoning = Reasoning::with_tools(
        "",
        vec![
            ToolCall::new("slow", "Slow", json!({})),
            ToolCall::new("fast", "Fast", json!({})),
        ],
    );
    let catalog = ctx
        .runtime
        .tool_catalog
        .pin_working_tools(&ctx.runtime.tools.read())
        .unwrap();
    let cancel = CancellationToken::new();
    let dispatch = dispatch_tools(&ctx, &reasoning, &catalog, &cancel);
    tokio::pin!(dispatch);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            tokio::select! {
                result = &mut dispatch => panic!("slow tool must still block the batch: {}", result.is_ok()),
                event = handles.render_rx.recv() => {
                    if matches!(event, Some(RenderEvent::ToolEnded { tool_call_id, .. }) if tool_call_id == "fast") { break; }
                }
            }
        }
    }).await.expect("fast completion must be observable while slow invoke waits");
    assert_eq!(
        transcript.read().len(),
        1,
        "no partial batch may become visible"
    );
    assert!(
        trace.lock().is_empty(),
        "after_tool waits for all concurrent invocations"
    );
    release.notify_one();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), dispatch)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome
            .results
            .iter()
            .map(|(call, _)| call.id.as_str())
            .collect::<Vec<_>>(),
        vec!["slow", "fast"]
    );
    assert_eq!(
        *trace.lock(),
        vec![("after_tool", 1), ("after_tool", 1), ("after_batch", 4)]
    );
    assert_eq!(transcript.read().len(), 4);
}
