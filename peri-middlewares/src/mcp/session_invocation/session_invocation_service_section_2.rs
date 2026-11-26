/// 分发失败 → typed 结果事实（不把「未执行」伪装成执行失败）。
///
/// 「无人可答」的**唯一**来源是错误自带的 typed 事实（[`EffectiveToolError::unanswered_cause`]，
/// 裁决 R9）：不再读任何带外记录点，任何消费方都拿到同一份事实。
pub(super) fn map_dispatch_error(
    error: EffectiveToolError,
    session_cancelled: bool,
) -> SessionInvocationOutcome {
    if let Some(cause) = error.unanswered_cause() {
        // 无人可答 ≠ 用户拒绝（两者在 outcome 记录层必须可区分）。
        return SessionInvocationOutcome::NotExecuted {
            cause: NotExecutedCause::ApprovalUnavailable(cause),
        };
    }
    match error.code {
        EffectiveToolErrorCode::UserRejected => SessionInvocationOutcome::NotExecuted {
            cause: NotExecutedCause::UserRejected,
        },
        // 策略层拒绝（hook 策略 / 分类器 / 审批机制缺失或超时 / 审批中间件在无审批
        // 模式下拒绝）：目标仍有效、也没有用户决定发生，不得记成「目标已撤销」，
        // 也不得记成「用户拒绝」。
        EffectiveToolErrorCode::PermissionDenied => SessionInvocationOutcome::NotExecuted {
            cause: NotExecutedCause::PolicyDenied,
        },
        EffectiveToolErrorCode::UnknownTool => SessionInvocationOutcome::NotExecuted {
            cause: NotExecutedCause::TargetUnbound,
        },
        EffectiveToolErrorCode::Cancelled if !session_cancelled => {
            SessionInvocationOutcome::NotExecuted {
                cause: NotExecutedCause::CancelledBeforeCommit,
            }
        }
        _ => match error.remote_error.and_then(|remote| remote.jsonrpc) {
            Some(jsonrpc) => SessionInvocationOutcome::RemoteProtocolError(jsonrpc),
            None => SessionInvocationOutcome::OutcomeUnknown {
                reason: error.message,
            },
        },
    }
}

#[async_trait]
impl SessionInvocationPort for SessionInvocationService {
    async fn invoke(
        &self,
        request: SessionInvocationRequest,
    ) -> Result<SessionInvocationOutcome, McpAppsRelayError> {
        if request.session_id != self.session_id {
            // 会话不匹配不是一次调用：不产生执行身份，也不写 outcome。
            return Err(McpAppsRelayError {
                kind: McpAppsErrorKind::InvalidSession,
            });
        }
        let invocation_id = request
            .invocation_id
            .clone()
            .unwrap_or_else(InvocationId::generate);
        let (outcome, skips, target) = self.run(&request, &invocation_id).await;
        Ok(self.finish(invocation_id, target, outcome, skips))
    }

    fn source_target(&self, session_id: &str, source: &InvocationId) -> Option<TargetIdentity> {
        // 会话不符不是「查不到记录」：本服务只保留本会话的调用结果，跨会话一律
        // 显式不可导出（ID 只定位，不授权）。
        if session_id != self.session_id {
            return None;
        }
        self.outcomes
            .snapshot()
            .into_iter()
            .rev()
            .find(|record| &record.invocation_id == source)
            .and_then(|record| record.target)
    }

    fn session_closing(&self) {
        self.session_cancel.cancel();
        let attachment_ids = self.registry.revoke_session(&self.session_id);
        self.admission.revoke(RevocationKey::Session);
        for attachment_id in attachment_ids {
            self.admission
                .revoke(RevocationKey::Attachment(attachment_id));
        }
    }

    fn revoke_app(&self, app_id: &str) {
        self.registry.revoke_app(app_id);
        self.admission
            .revoke(RevocationKey::App(app_id.to_string()));
    }

    fn revoke_attachment(&self, attachment_id: &AttachmentId) {
        // 附件级撤销**只**作用于该附件（裁决 D6）：按附件身份定位持有它的记录，
        // 只摘掉这一个附件；record 本体与其他附件都不受影响（连接关闭不再是
        // record 的终态撤销）。附件身份与 record 身份是两类身份，不得互相代替。
        if let Some(record) = self.registry.record_for_attachment(attachment_id) {
            record.detach_attachment(attachment_id);
        }
        // 附件级准入键：撤掉「该附件名下未提交调用」的准入资格。注意 App 调用的
        // 提交键是 `App(record_id)`（见 `bind_app_target`），因此本行只对未来
        // 以附件为键的准入面生效；已提交调用不受影响（撤销不取消执行）。
        self.admission
            .revoke(RevocationKey::Attachment(attachment_id.clone()));
    }
}

// ─── 脚本工具调用通道（issue #407）：宿主侧实现 ─────────────────────────────
//
// 脚本只给 effective 名与对象参数；本实现负责解析、审批与执行（同一 canonical
// 管线），并把结果事实映射成脚本错误码。不写 outcome store、不重复 schema 校验。

#[async_trait]
impl WorkflowToolInvoker for SessionInvocationService {
    async fn invoke_tool(&self, call: WorkflowToolCall) -> WorkflowToolOutcome {
        let tool = call.tool_name.clone();
        let Some(bridge) = self.resolve_visible_effective(&call.tool_name) else {
            return WorkflowToolOutcome::error(
                WorkflowToolErrorCode::ToolNotFound,
                tool,
                format!(
                    "tool '{}' is not visible in this session (unknown or ambiguous effective name)",
                    call.tool_name
                ),
            );
        };
        let (Some(server_id), Some(wire_tool_name)) = (
            bridge.mcp_server_name().map(str::to_string),
            bridge.mcp_tool_name().map(str::to_string),
        ) else {
            return WorkflowToolOutcome::error(
                WorkflowToolErrorCode::ToolNotFound,
                tool,
                "resolved tool has no MCP server/tool identity",
            );
        };
        // alias 归一复用 canonical `normalize_params`（传入解析出的 bridge）；这里
        // 不重复 schema 校验（归 MCP 服务端）。
        let normalized = peri_agent::tools::invocation::normalize_params(
            serde_json::Value::Object(call.arguments),
            Some(&bridge as &dyn BaseTool),
        );
        let serde_json::Value::Object(arguments) = normalized else {
            return WorkflowToolOutcome::error(
                WorkflowToolErrorCode::InvalidArguments,
                tool,
                "tool arguments must be a JSON object",
            );
        };
        let outcome = self
            .invoke_unrecorded(
                InvocationOrigin::Host {
                    namespace: "workflow".to_string(),
                    seq: call.seq,
                },
                InvocationTarget::Host {
                    server_id,
                    tool_name: wire_tool_name,
                    arguments,
                },
                call.cancel,
            )
            .await;
        map_workflow_tool_outcome(outcome, &tool)
    }
}

/// `SessionInvocationOutcome` → 脚本可见结果（不把未执行伪装成执行失败）。
pub(crate) fn map_workflow_tool_outcome(
    outcome: SessionInvocationOutcome,
    tool: &str,
) -> WorkflowToolOutcome {
    match outcome {
        SessionInvocationOutcome::Completed(raw) => {
            let text = raw_text_content(&raw);
            if raw.is_error() {
                let message = if text.is_empty() {
                    "tool returned an error result".to_string()
                } else {
                    text
                };
                return WorkflowToolOutcome::error_with_details(
                    WorkflowToolErrorCode::ToolError,
                    tool,
                    message,
                    raw.as_value().clone(),
                );
            }
            WorkflowToolOutcome::Ok {
                structured_content: raw.structured_content().cloned(),
                content: text,
            }
        }
        SessionInvocationOutcome::RemoteProtocolError(error) => {
            let details = serde_json::to_value(&error).ok();
            match details {
                Some(details) => WorkflowToolOutcome::error_with_details(
                    WorkflowToolErrorCode::ProtocolError,
                    tool,
                    error.message.clone(),
                    details,
                ),
                None => WorkflowToolOutcome::error(
                    WorkflowToolErrorCode::ProtocolError,
                    tool,
                    error.message,
                ),
            }
        }
        SessionInvocationOutcome::TaskAccepted(receipt) => {
            let details = serde_json::to_value(&receipt).ok();
            let message = format!(
                "MCP server accepted the call as a task ({}); this workflow version does not poll tasks",
                receipt.status
            );
            match details {
                Some(details) => WorkflowToolOutcome::error_with_details(
                    WorkflowToolErrorCode::TaskAccepted,
                    tool,
                    message,
                    details,
                ),
                None => {
                    WorkflowToolOutcome::error(WorkflowToolErrorCode::TaskAccepted, tool, message)
                }
            }
        }
        SessionInvocationOutcome::OutcomeUnknown { reason } => {
            WorkflowToolOutcome::error(WorkflowToolErrorCode::OutcomeUnknown, tool, reason)
        }
        SessionInvocationOutcome::NotExecuted { cause } => {
            WorkflowToolOutcome::error(workflow_tool_error_code(cause), tool, format!("{cause:?}"))
        }
    }
}

/// 未执行原因 → 脚本错误码（审批不可用 / 策略拒绝 / 取消 / 未执行四类显式分离）。
fn workflow_tool_error_code(cause: NotExecutedCause) -> WorkflowToolErrorCode {
    match cause {
        NotExecutedCause::ApprovalUnavailable(_) => WorkflowToolErrorCode::ApprovalUnavailable,
        NotExecutedCause::UserRejected
        | NotExecutedCause::PolicyDenied
        | NotExecutedCause::TargetRevoked
        | NotExecutedCause::CommitConsumed => WorkflowToolErrorCode::PolicyDenied,
        NotExecutedCause::CancelledBeforeCommit => WorkflowToolErrorCode::Cancelled,
        NotExecutedCause::TargetUnbound | NotExecutedCause::UntrustedOwner => {
            WorkflowToolErrorCode::NotExecuted
        }
    }
}

/// 结果正文的文本投影：只拼接 `content[].text`（图像 / 资源块经
/// `structuredContent` 或 `details` 保真承载，不在这里编造文本）。
fn raw_text_content(raw: &peri_acp_types::mcp_apps::RawCallToolResult) -> String {
    raw.content()
        .iter()
        .filter_map(|block| block.get("text").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}
