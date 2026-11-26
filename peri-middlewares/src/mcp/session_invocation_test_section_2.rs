/// [D4] rerun 目标权威：目标身份来自**服务端 outcome 记录**；跨会话 / 未保留 /
/// 未绑定目标的调用一律显式不可导出（绝不回落客户端字段）。
#[tokio::test]
async fn source_target_exports_target_from_retained_outcome_record() {
    let fixture = Fixture::new(
        InteractionResponse::Decisions(vec![
            peri_acp_types::interaction::ApprovalDecision::Approve { source: None },
        ]),
        None,
    )
    .await;
    let record = fixture.attach_record("connection-1");

    let outcome = fixture
        .service
        .invoke(fixture.app_request(&record, serde_json::json!({"value": 1})))
        .await
        .expect("请求本身合法");
    assert!(matches!(outcome, SessionInvocationOutcome::Completed(_)));

    let retained = fixture
        .service
        .outcomes()
        .latest()
        .expect("outcome 必须入账");
    let target = fixture
        .service
        .source_target(SESSION, &retained.invocation_id)
        .expect("已执行调用必须能从服务端记录导出目标");
    assert_eq!(target.server_id, SERVER);
    assert_eq!(target.wire_tool_name, APP_TOOL, "wire 名是服务端声明名");
    assert_eq!(
        target.effective_tool_name,
        super::super::tool_bridge::effective_mcp_tool_name(SERVER, APP_TOOL),
        "effective 名是目录准入后的名字（两者不可互相代替）"
    );

    // 跨会话：本服务只导出本会话记录（ID 只定位，不授权）。
    assert!(
        fixture
            .service
            .source_target("other-session", &retained.invocation_id)
            .is_none(),
        "跨会话不得导出目标"
    );
    // 未知 / 已淘汰的执行身份：显式无目标。
    assert!(
        fixture
            .service
            .source_target(SESSION, &InvocationId::generate())
            .is_none(),
        "未保留的执行身份不得导出目标"
    );

    // 绑定目标**之前**被拒的调用：记录里没有目标权威，同样不可导出。
    let (attachment_id, epoch) = fixture
        .attachment_facts
        .lock()
        .get(&record.record_id)
        .cloned()
        .expect("夹具自检：附件事实必须记录");
    let rejected = fixture
        .service
        .invoke(fixture.app_request_from(
            &record,
            attachment_id,
            epoch.next(),
            serde_json::json!({}),
        ))
        .await
        .expect("请求本身合法");
    assert_eq!(
        rejected,
        SessionInvocationOutcome::NotExecuted {
            cause: NotExecutedCause::UntrustedOwner
        },
        "纪元不符必须显式未执行"
    );
    let rejected_record = fixture
        .service
        .outcomes()
        .latest()
        .expect("拒绝同样要入账（不静默）");
    assert!(
        rejected_record.target.is_none(),
        "未绑定目标的调用不得伪造目标身份"
    );
    assert!(
        fixture
            .service
            .source_target(SESSION, &rejected_record.invocation_id)
            .is_none(),
        "无目标的记录必须显式不可导出"
    );
    fixture.shutdown().await;
}

/// `map_dispatch_error` 的 typed 事实**只**来自错误本身（裁决 R9）：不再有带外
/// 「无人可答」记录点，两种原因各自如实落账。
#[test]
fn map_dispatch_error_reads_typed_cause_without_side_channel() {
    let unavailable = peri_agent::tools::EffectiveToolError::new(
        EffectiveToolErrorCode::PermissionDenied,
        UnansweredCause::OwnerUnmatched.reason_text(),
    )
    .with_unanswered_cause(UnansweredCause::OwnerUnmatched);
    assert_eq!(
        super::map_dispatch_error(unavailable, false),
        SessionInvocationOutcome::NotExecuted {
            cause: NotExecutedCause::ApprovalUnavailable(UnansweredCause::OwnerUnmatched)
        },
        "「没有人可以决定」必须落成 ApprovalUnavailable，不得折叠成用户拒绝"
    );

    let rejected = peri_agent::tools::EffectiveToolError::new(
        EffectiveToolErrorCode::UserRejected,
        "用户拒绝",
    );
    assert_eq!(
        super::map_dispatch_error(rejected, false),
        SessionInvocationOutcome::NotExecuted {
            cause: NotExecutedCause::UserRejected
        },
        "已作出的拒绝决定必须落成 UserRejected"
    );

    // 本端策略拒绝（hook / 分类器 / 审批机制缺失或超时）：既不是用户拒绝，
    // 也不是无人可答，且不得记成「目标已撤销」。
    let policy = peri_agent::tools::EffectiveToolError::new(
        EffectiveToolErrorCode::PermissionDenied,
        "Hook blocked this call",
    );
    assert_eq!(
        super::map_dispatch_error(policy, false),
        SessionInvocationOutcome::NotExecuted {
            cause: NotExecutedCause::PolicyDenied
        }
    );
}

// ─── 脚本工具通道（issue #407）─────────────────────────────────────────────

/// 构造一个只含指定工具的 client（builtin 来源，走声明的 model_visible 判定）。
fn install_builtin_client(pool: &McpClientPool, instance: &str, tools: Vec<rmcp::model::Tool>) {
    let handle = Arc::new(super::super::client::McpClientHandle {
        call_gate: Default::default(),
        name: instance.to_string(),
        version: None,
        connected_at: None,
        protocol_version: None,
        cache_version: None,
        peer: None,
        tools,
        resources: vec![],
        status: super::super::client::ClientStatus::Connected,
        oauth_status: Default::default(),
        source: Some(super::super::config::ConfigSource::Builtin {
            instance: instance.to_string(),
        }),
        url: None,
        skills_capable: false,
    });
    pool.advance_handle_generation(&handle);
    pool.clients.write().insert(instance.to_string(), handle);
}

fn plain_tool(name: &str) -> rmcp::model::Tool {
    serde_json::from_value(serde_json::json!({
        "name": name,
        "description": name,
        "inputSchema": {"type": "object"}
    }))
    .expect("夹具工具声明必须可解析")
}

fn resolution_service(
    workflow_tools: Vec<rmcp::model::Tool>,
    app_tools: Vec<rmcp::model::Tool>,
    closed: std::collections::BTreeSet<String>,
) -> Arc<SessionInvocationService> {
    let pool = Arc::new(McpClientPool::new_empty());
    install_builtin_client(&pool, "workflow", workflow_tools);
    install_builtin_client(&pool, "app-server", app_tools);
    Arc::new(SessionInvocationService::new(SessionInvocationInputs {
        pool,
        agent_id: peri_acp_types::identity::AgentId::new(),
        session_id: SESSION.to_string(),
        cwd: std::sync::Arc::from("/tmp/app-session"),
        queue: Arc::new(MessageQueue::new),
        rules: Arc::new(InvocationRuleChain::from_derived_blueprint(
            MiddlewareChain::new(),
        )),
        closed_instances: closed,
        session_cancel: CancellationToken::new(),
    }))
}

/// effective 名解析在 `session_visible_bridges` 同一视图内：deferred 可见、
/// host-only 不可见、workflow 实例自身不可见、关闭实例不可见、重名 fail closed。
#[test]
fn visible_effective_resolution_covers_deferred_and_fails_closed() {
    let service = resolution_service(
        vec![
            // workflow 实例自身：即使模型可见/延迟目录可见，脚本也不得解析。
            plain_tool("WorkflowStatus"),
            // host-only：脚本不可见（与模型目录同一可见性判定）。
            plain_tool("WorkflowAgentResult"),
        ],
        vec![
            // 外部 deferred（builtin 夹具声明 `direct: false`）但模型可见。
            plain_tool("show_canvas"),
        ],
        Default::default(),
    );
    let deferred = service
        .resolve_visible_effective("mcp__app-server__show_canvas")
        .expect("外部 deferred 工具必须在解析视图内");
    assert_eq!(deferred.mcp_server_name(), Some("app-server"));
    assert_eq!(deferred.mcp_tool_name(), Some("show_canvas"));
    assert!(
        service
            .resolve_visible_effective("mcp__workflow__WorkflowStatus")
            .is_none(),
        "workflow 实例自身的工具不得被脚本解析"
    );
    assert!(
        service
            .resolve_visible_effective("mcp__workflow__WorkflowAgentResult")
            .is_none(),
        "host-only 工具不得被脚本解析"
    );
    assert!(service.resolve_visible_effective("Read").is_none());

    // 关闭实例：同一视图排除（与生产 MCP middleware 同一关闭集）。
    let mut closed = std::collections::BTreeSet::new();
    closed.insert("app-server".to_string());
    let closed_service = resolution_service(vec![], vec![plain_tool("show_canvas")], closed);
    assert!(
        closed_service
            .resolve_visible_effective("mcp__app-server__show_canvas")
            .is_none(),
        "关闭实例的工具不得被解析"
    );

    // 重名（同一 server 声明两个同名工具）：不猜目标，fail closed。
    let duplicate = resolution_service(
        vec![],
        vec![plain_tool("show_canvas"), plain_tool("show_canvas")],
        Default::default(),
    );
    assert!(
        duplicate
            .resolve_visible_effective("mcp__app-server__show_canvas")
            .is_none(),
        "重名 effective 名必须 fail closed"
    );
}

/// 脚本通道真实执行 canonical 管线，但不写 session outcome store。
#[tokio::test]
async fn workflow_tool_invoker_runs_without_recording_an_outcome() {
    let fixture = Arc::new(
        Fixture::new(
            InteractionResponse::Decisions(vec![
                peri_acp_types::interaction::ApprovalDecision::Approve { source: None },
            ]),
            None,
        )
        .await,
    );
    let outcome = fixture
        .service
        .invoke_tool(WorkflowToolCall {
            run_id: "run-tool".into(),
            task_id: "tool-1".into(),
            seq: 0,
            tool_name: "mcp__app-server__show_canvas".into(),
            arguments: Default::default(),
            cancel: CancellationToken::new(),
        })
        .await;
    match outcome {
        WorkflowToolOutcome::Ok {
            structured_content,
            content,
        } => {
            assert!(structured_content.is_none());
            assert_eq!(content, "canvas ready");
        }
        other => panic!("成功工具调用必须回填 ok，实际: {other:?}"),
    }
    assert_eq!(calls(&fixture), 1, "真实 peer 恰好收到一次调用");
    assert!(
        fixture.service.outcomes().is_empty(),
        "脚本工具路径不得写 session outcome store"
    );
    // 未知 effective 名：零 wire 调用，返回可 catch 的类型化错误。
    let missing = fixture
        .service
        .invoke_tool(WorkflowToolCall {
            run_id: "run-tool".into(),
            task_id: "tool-2".into(),
            seq: 1,
            tool_name: "mcp__app-server__missing".into(),
            arguments: Default::default(),
            cancel: CancellationToken::new(),
        })
        .await;
    assert!(matches!(
        missing,
        WorkflowToolOutcome::Error {
            code: WorkflowToolErrorCode::ToolNotFound,
            ..
        }
    ));
    assert_eq!(calls(&fixture), 1);
    Arc::try_unwrap(fixture)
        .ok()
        .expect("fixture 只被测试本身持有")
        .shutdown()
        .await;
}

/// 结果事实 → 脚本错误码的完整映射（不把未执行伪装成执行失败）。
#[test]
fn workflow_tool_outcome_mapping_is_typed() {
    use peri_acp_types::mcp_apps::{
        JsonRpcError, RawCallToolResult, SessionInvocationOutcome, TaskReceipt,
    };

    let ok = super::map_workflow_tool_outcome(
        SessionInvocationOutcome::Completed(RawCallToolResult::new(serde_json::json!({
            "content": [{"type": "text", "text": "hello"}],
            "structuredContent": {"value": 7}
        }))),
        "Read",
    );
    assert_eq!(
        ok,
        WorkflowToolOutcome::Ok {
            structured_content: Some(serde_json::json!({"value": 7})),
            content: "hello".into(),
        }
    );

    let failed = super::map_workflow_tool_outcome(
        SessionInvocationOutcome::Completed(RawCallToolResult::new(serde_json::json!({
            "content": [{"type": "text", "text": "boom"}],
            "isError": true
        }))),
        "Read",
    );
    assert!(matches!(
        failed,
        WorkflowToolOutcome::Error {
            code: WorkflowToolErrorCode::ToolError,
            ref message,
            ..
        } if message == "boom"
    ));

    let protocol = super::map_workflow_tool_outcome(
        SessionInvocationOutcome::RemoteProtocolError(JsonRpcError {
            code: -32603,
            message: "internal".into(),
            data: None,
        }),
        "Read",
    );
    assert!(matches!(
        protocol,
        WorkflowToolOutcome::Error {
            code: WorkflowToolErrorCode::ProtocolError,
            ..
        }
    ));

    let accepted = super::map_workflow_tool_outcome(
        SessionInvocationOutcome::TaskAccepted(TaskReceipt {
            task_id: "remote-task".into(),
            status: "working".into(),
        }),
        "Read",
    );
    assert!(matches!(
        accepted,
        WorkflowToolOutcome::Error {
            code: WorkflowToolErrorCode::TaskAccepted,
            ..
        }
    ));
    assert!(accepted.marks_unknown_side_effects());

    let unknown = super::map_workflow_tool_outcome(
        SessionInvocationOutcome::OutcomeUnknown {
            reason: "no raw payload".into(),
        },
        "Read",
    );
    assert!(matches!(
        unknown,
        WorkflowToolOutcome::Error {
            code: WorkflowToolErrorCode::OutcomeUnknown,
            ..
        }
    ));
    assert!(unknown.marks_unknown_side_effects());

    for (cause, expected) in [
        (
            NotExecutedCause::ApprovalUnavailable(UnansweredCause::NoOnlineEndpoint),
            WorkflowToolErrorCode::ApprovalUnavailable,
        ),
        (
            NotExecutedCause::UserRejected,
            WorkflowToolErrorCode::PolicyDenied,
        ),
        (
            NotExecutedCause::PolicyDenied,
            WorkflowToolErrorCode::PolicyDenied,
        ),
        (
            NotExecutedCause::TargetRevoked,
            WorkflowToolErrorCode::PolicyDenied,
        ),
        (
            NotExecutedCause::CancelledBeforeCommit,
            WorkflowToolErrorCode::Cancelled,
        ),
        (
            NotExecutedCause::TargetUnbound,
            WorkflowToolErrorCode::NotExecuted,
        ),
    ] {
        let mapped = super::map_workflow_tool_outcome(
            SessionInvocationOutcome::NotExecuted { cause },
            "Read",
        );
        assert!(
            matches!(mapped, WorkflowToolOutcome::Error { code, .. } if code == expected),
            "{cause:?} 必须映射为 {expected:?}，实际: {mapped:?}"
        );
        assert!(!mapped.marks_unknown_side_effects());
    }
}
