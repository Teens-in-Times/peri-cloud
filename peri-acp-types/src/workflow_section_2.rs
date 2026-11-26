fn post_condition_word(status: PostProcessingStatus) -> &'static str {
    match status {
        // read_only 通过 = 仓库前后未被改动，读作「已验证」而不是「未处理」。
        PostProcessingStatus::NotRequired => "passed (read-only verified)",
        PostProcessingStatus::Passed => "passed (write intent verified)",
        PostProcessingStatus::Failed => "failed",
        // 无基线、采样失败与无法归因都落在这里：只陈述“未验证”，不猜测原因。
        PostProcessingStatus::Blocked => "blocked (post-condition not verified)",
        PostProcessingStatus::Unknown => "unknown",
    }
}

fn delivery_status_word(status: DeliveryStatus) -> &'static str {
    match status {
        DeliveryStatus::Blocked => "blocked",
        DeliveryStatus::Unknown => "unknown",
    }
}

fn outcome_word(outcome: WorkflowOutcome) -> &'static str {
    match outcome {
        WorkflowOutcome::Completed => "completed (engine-level; acceptance not judged)",
        WorkflowOutcome::Blocked => "blocked",
        WorkflowOutcome::Failed => "failed",
        WorkflowOutcome::Killed => "killed",
        WorkflowOutcome::Unknown => "unknown",
    }
}

/// `return_value` 长文本外置阈值（**字节**，非字符数）：超过它的字符串在写
/// `state.json` 前被抽出到 `outputs/<label>.txt`，原位替换为 `${label}` 引用。
///
/// 单一权威：宿主提取的调用点（`peri-workflow` 的 `extract_long_texts` 调用）
/// 与提醒/启动文案（`WorkflowTaskResult::to_notification`、
/// `peri-workflow` 的启动文案）共用本常量。CJK 一字 3 字节，所以「200 字节」
/// 对中文内容远早于 200 字触发；改这里即同时改变提取行为与对外说明。
/// 口径见 `docs/design/workflow.md` §3.6。
///
/// 作用面：宿主提取点位于 `workflow/done` 处理，对任意 `return` 形态生效——
/// 对象字段 / 数组元素 / 顶层字符串都会被外置（顶层字符串的 label 固定
/// `return`，数组元素用 `[i]` / `<key>[i]`）。
pub const LONGTEXT_EXTRACT_THRESHOLD_BYTES: usize = 200;

impl WorkflowTaskResult {
    /// Canonical background task projection, shared by owner and MCP consumer.
    pub fn background_result(&self) -> crate::event::BackgroundTaskResult {
        crate::event::BackgroundTaskResult {
            task_id: self.run_id.clone(),
            agent_name: format!("workflow:{}", self.workflow_name),
            prompt_summary: self.workflow_name.clone(),
            success: self.agent_facing_success(),
            output: self.to_notification(),
            tool_calls_count: self.tool_calls_count,
            duration_ms: self.duration_ms,
            child_thread_id: None,
            timed_out: false,
            subagent_failure: None,
            shell_output: None,
        }
    }

    /// 主 Agent 是否应把 workflow 视为「成功交付」（非仅 engine 跑完）。
    #[must_use]
    pub fn agent_facing_success(&self) -> bool {
        self.outcome == WorkflowOutcome::Completed
    }

    /// Defer / 通知用状态短语：`delivery_status=blocked` 时不得写 "completed"。
    pub fn notification_status_phrase(&self) -> &'static str {
        match self.status {
            WorkflowRunStatus::Running => "running",
            WorkflowRunStatus::Completed => {
                if self.delivery_status == DeliveryStatus::Blocked {
                    "finished (engine completed; delivery blocked)"
                } else {
                    "completed"
                }
            }
            WorkflowRunStatus::Killed => "killed",
            WorkflowRunStatus::Failed => "failed",
        }
    }

    /// 格式化 workflow 完成通知正文，含 phase breakdown。
    ///
    /// 本函数不编码 `<system-reminder>` envelope；canonical producer 在队列/投影边界
    /// 统一编码，避免 producer 直接拼接 wire format。
    pub fn to_notification(&self) -> String {
        let success_msg = self.notification_status_phrase();

        let mut phase_lines = String::new();
        if !self.phase_summaries.is_empty() {
            for s in &self.phase_summaries {
                let token_info = if s.token_count > 0 {
                    format!(", {} tokens", s.token_count)
                } else {
                    String::new()
                };
                let dur_info = if let Some(d) = s.duration_ms {
                    format!(", {}ms", d)
                } else {
                    String::new()
                };
                phase_lines.push_str(&format!(
                    "- {}: {} agents{}{}\n",
                    escape_reminder_text(&s.name),
                    s.agent_count,
                    token_info,
                    dur_info
                ));
            }
        }

        let error_line = if self.status == WorkflowRunStatus::Failed
            || self.delivery_status == DeliveryStatus::Blocked
            || self
                .error
                .as_deref()
                .is_some_and(|error| !error.trim().is_empty())
        {
            let error = self
                .error
                .as_deref()
                .filter(|error| !error.trim().is_empty())
                .unwrap_or(if self.delivery_status == DeliveryStatus::Blocked {
                    "delivery is blocked; inspect the post-condition and required evidence"
                } else {
                    "no error details available"
                });
            let error = escape_reminder_text(&crate::session::bounded_error_message(error, 2_000));
            format!("Error: {error}\n")
        } else {
            String::new()
        };
        let state_path = format!(
            ".claude/workflow-runs/{}/state.json",
            escape_reminder_text(&self.run_id)
        );
        let (result_lines, inline_complete) = match &self.result_preview {
            Some(preview) => {
                let (text, complete) = preview.bounded_text();
                let completeness = if complete {
                    "complete"
                } else {
                    "partial; not a complete result"
                };
                (format!(
                    "Reported result ({completeness}; workflow output, not an engine acceptance verdict):\n{}\n",
                    escape_reminder_text(text)
                ), complete)
            }
            None => (String::new(), false),
        };
        let artifact_lines = if self.state_artifact_exists && inline_complete {
            format!(
                "Full artifacts saved to {state_path}. Use the inline result when it contains the \
                 evidence needed for the handoff; do not restart agents just to repeat their report.\n"
            )
        } else if self.state_artifact_exists {
            // 指引指代 builtin 文件工具时必须用**模型面名字**（裸名已无提供面）；
            // 查表未命中的兜底不含工具名，不得回落到裸名。
            // `${label}` 是写入期契约（阈值见 LONGTEXT_EXTRACT_THRESHOLD_BYTES）：
            // 不说明它是引用，读 state.json 的消费方会把 token 当结果、静默丢长文本。
            let read_lead = crate::builtin_mcp::model_tool_name_of("workspace", "Read")
                .map_or("Read it".to_string(), |name| format!("Use `{name}`"));
            let threshold = LONGTEXT_EXTRACT_THRESHOLD_BYTES;
            format!(
                "Results saved to {state_path}\n\
                 Long strings in the result (over {threshold} bytes, including a top-level \
                 string or array items) are externalized: \
                 a `${{label}}` token in state.json is that value's path, and its text lives in \
                 `outputs/` under the same label with a `.txt` suffix.\n\
                 {read_lead} to view full results.\n"
            )
        } else {
            "Result state file was not generated.\n".to_string()
        };
        let projection_line = format!(
            "Execution: {}; outcome: {}; acceptance: {}; post-condition: {}; delivery: {}.\n\
             Acceptance is judged outside the engine, so it reads `unknown` unless the calling \
             layer (ADLC/skill) supplies a verdict; outcome `completed` is engine-level and does \
             not mean task success.\n",
            execution_status_word(self.execution_status),
            outcome_word(self.outcome),
            acceptance_status_word(self.acceptance_status),
            post_condition_word(self.post_processing_status),
            delivery_status_word(self.delivery_status),
        );
        // 未知副作用是观测事实：只在 true 时出现，且明确要求核对外部影响。
        let unknown_side_effects_line = if self.unknown_side_effects {
            "Unknown tool side effects: at least one script tool call had no settled result; \
             verify external effects before relying on this run.\n"
        } else {
            ""
        };

        let attempt_lines = self
            .attempts
            .iter()
            .map(|attempt| {
                format!(
                    "- attempt run_id={} agent_id={} journal_seq={} disposition={:?} consumed={}{}\n",
                    escape_reminder_text(&attempt.run_id),
                    attempt.agent_id.map_or("unknown".to_string(), |id| id.to_string()),
                    attempt.journal_seq,
                    attempt.disposition,
                    attempt.consumed,
                    attempt.recovered_from.as_ref().map_or_else(String::new, |source| {
                        format!(
                            " recovered_from={}/{}/{}",
                            escape_reminder_text(&source.run_id),
                            source
                                .agent_id
                                .map_or("unknown".to_string(), |id| id.to_string()),
                            source.journal_seq
                        )
                    })
                )
                .to_ascii_lowercase()
            })
            .collect::<String>();

        format!(
            "Workflow '{}' {}. ({}ms, {} agents, {} tool calls)\n\
            {}{}{}{}{}{}{}",
            escape_reminder_text(&self.workflow_name),
            success_msg,
            self.duration_ms,
            self.agent_count,
            self.tool_calls_count,
            phase_lines,
            error_line,
            projection_line,
            unknown_side_effects_line,
            attempt_lines,
            result_lines,
            artifact_lines,
        )
    }
}

/// Agent 回调执行器 trait（由 peri-acp 实现）
#[async_trait]
pub trait AgentExecutor: Send + Sync {
    /// Bind the session task manager before any workflow agent is admitted.
    fn bind_execution_manager(
        &self,
        _manager: std::sync::Arc<dyn crate::tasks::TaskManager>,
    ) -> Result<(), String> {
        Ok(())
    }
    /// 执行单个 workflow agent，返回 AgentRunResult
    async fn execute(&self, params: AgentRunParams) -> AgentRunResult;

    /// 本次执行实际解析到的硬上限与用量（宿主结算路径读取）。
    ///
    /// 默认未知：限额数值只由执行器在解析后给出，宿主不得自行推断或写死默认值。
    fn execution_limits(&self, _run_id: &str, _agent_id: u64) -> Option<AgentExecutionLimits> {
        None
    }

    /// run 结束时回收该 run 的执行事实（无状态实现无需覆盖）。
    fn forget_run(&self, _run_id: &str) {}
}

// ─── 脚本工具调用（`tool()` 原语；阶段 1 宿主侧契约）───────────────────────
//
// 脚本只给 effective 名与 `arguments` 对象；effective 名到 target 的解析、审批与
// 执行全部归宿主（会话可见视图 + canonical 调用管线）。本模块只声明跨层契约与
// wire 形状，不重复 MCP 服务端的 schema 校验，也不承载第二套审批实现。

/// 一次脚本发起的 MCP 工具调用（宿主消费者 claim 后交给 [`WorkflowToolInvoker`]）。
#[derive(Debug, Clone)]
pub struct WorkflowToolCall {
    /// 所属 run（回填身份；`taskId` 作用域 = 单次 run）。
    pub run_id: String,
    /// 任务表主键（回填 `WorkflowToolResult` 的身份）。
    pub task_id: String,
    /// run 内的稳定调用序号（`InvocationOrigin::Host{namespace:"workflow", seq}`）。
    pub seq: u64,
    /// 脚本给出的 effective 工具名（不含旧前缀归一）。
    pub tool_name: String,
    /// 已通过对象形状检查的调用参数。
    pub arguments: serde_json::Map<String, Value>,
    /// 任务取消令牌：**只**作为准入前的放弃信号（已 dispatch 的调用不强行中断）。
    pub cancel: tokio_util::sync::CancellationToken,
}

/// 脚本工具调用的宿主实现（由会话装配注入 workflow consumer）。
#[async_trait]
pub trait WorkflowToolInvoker: Send + Sync {
    /// 在会话可见视图内解析 effective 名、走 canonical 管线并返回结果事实。
    async fn invoke_tool(&self, call: WorkflowToolCall) -> WorkflowToolOutcome;
}

/// 脚本工具调用的终态事实（wire：`tool/result.outcome`，tagged `ok` / `error`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "kind"
)]
pub enum WorkflowToolOutcome {
    /// 工具已执行且未报告失败：`structuredContent` 优先，文本 content 兜底。
    Ok {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        structured_content: Option<Value>,
        #[serde(default)]
        content: String,
    },
    /// 失败：脚本侧按 `code` 抛类型化错误（可 `try/catch`）。
    Error {
        code: WorkflowToolErrorCode,
        /// effective 工具名（脚本传入的原值）。
        tool: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
}

impl WorkflowToolOutcome {
    #[must_use]
    pub fn error(
        code: WorkflowToolErrorCode,
        tool: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::Error {
            code,
            tool: tool.into(),
            message: message.into(),
            details: None,
        }
    }

    #[must_use]
    pub fn error_with_details(
        code: WorkflowToolErrorCode,
        tool: impl Into<String>,
        message: impl Into<String>,
        details: Value,
    ) -> Self {
        Self::Error {
            code,
            tool: tool.into(),
            message: message.into(),
            details: Some(details),
        }
    }

    /// 是否意味着「可能已经发生不可确认的副作用」。
    ///
    /// run 终态据此置 `unknownSideEffects`（不升级为 run 失败）：`outcome_unknown`
    /// 是已发出调用但结果不可知；`task_accepted` 是远端接受为异步任务而未给终态，
    /// v1 不轮询。
    #[must_use]
    pub fn marks_unknown_side_effects(&self) -> bool {
        matches!(
            self,
            Self::Error {
                code: WorkflowToolErrorCode::OutcomeUnknown | WorkflowToolErrorCode::TaskAccepted,
                ..
            }
        )
    }
}

/// 脚本可见的工具调用错误码（脚本 `WorkflowToolError.code`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowToolErrorCode {
    /// effective 名不在会话可见视图内（或重名无法唯一解析）。
    ToolNotFound,
    /// `arguments` 不是对象（Node 侧同口径拒绝）。
    InvalidArguments,
    /// 本部署形态没有脚本工具通道（非会话路径 / 实例关闭）。
    ToolUnavailable,
    /// 策略或调用授权拒绝（含目标撤销、非用户决定的拒绝）。
    PolicyDenied,
    /// 审批不可用（零 / 多在线 endpoint、epoch 过期、客户端不能作答）。
    ApprovalUnavailable,
    /// 显式未执行（准入在 wire 调用之前拒绝，零副作用）。
    NotExecuted,
    /// 工具返回了 `isError: true`。
    ToolError,
    /// 远端 JSON-RPC 协议错误（不是工具结果）。
    ProtocolError,
    /// 远端接受为异步任务；v1 不轮询。
    TaskAccepted,
    /// 已发出调用但结果不可知（不得当成成功）。
    OutcomeUnknown,
    /// 准入前取消 / 放弃。
    Cancelled,
}

#[cfg(test)]
#[path = "workflow_test.rs"]
mod tests;

#[cfg(test)]
mod scope_close_tests {
    use super::AgentRunResult;

    #[test]
    fn first_failure_and_scope_uncertainty_round_trip_independently() {
        for reason in [
            "event-forwarder-failed",
            "initial-prompt-enqueue-failed",
            "execution-blocked",
            "interrupted",
        ] {
            let original: AgentRunResult = serde_json::from_value(serde_json::json!({
                "kind": "dead", "reason": reason, "detail": "first cause"
            }))
            .unwrap();
            assert!(original.unsettled_scope_error().is_none());
            assert!(serde_json::to_value(&original)
                .unwrap()
                .get("scopeCloseIncomplete")
                .is_none());
            let closed = original.clone().after_scope_close(true);
            assert_eq!(closed, original);
            let unsettled = original.after_scope_close(false);
            assert!(unsettled.unsettled_scope_error().is_some());
            let wire = serde_json::to_value(&unsettled).unwrap();
            assert_eq!(wire["reason"], reason);
            assert_eq!(wire["detail"], "first cause");
            assert_eq!(wire["scopeCloseIncomplete"], true);
            let restored: AgentRunResult = serde_json::from_value(wire).unwrap();
            assert_eq!(restored, unsettled);
            assert!(restored.unsettled_scope_error().is_some());
        }
    }

    #[test]
    fn legacy_scope_failure_still_fences() {
        let legacy: AgentRunResult = serde_json::from_value(serde_json::json!({
            "kind": "dead", "reason": "scope-close-incomplete"
        }))
        .unwrap();
        assert!(legacy.unsettled_scope_error().is_some());
        assert!(AgentRunResult::Skipped
            .after_scope_close(false)
            .unsettled_scope_error()
            .is_some());
    }
}
