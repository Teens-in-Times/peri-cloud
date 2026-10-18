//! Manual context maintenance is a deployment-owned turn, not a chat prompt.
use super::*;

impl CloudAgent {
    pub(super) async fn compact_turn(
        &self,
        request: CloudTurnRequest,
        model: Arc<dyn Model>,
    ) -> CloudTurnResult {
        let mut transcript = MessageTranscript::new().with_own_payloads(request.history);
        transcript.set_flags_batch(request.history_flags);
        let before = transcript.visible_messages().len();
        let empty = transcript.canonical_payloads().iter().all(|entry| {
            transcript.flags(entry.id()).excluded
                || crate::memory::frames::is_frame(entry)
                || matches!(entry.as_message(), Some(BaseMessage::System { .. }))
        });
        let (status, text) = if empty {
            (
                TurnStatus::Completed,
                "当前会话没有历史可压缩，没有调用模型。".to_owned(),
            )
        } else {
            // A user-triggered command makes one bounded summary attempt. The
            // v4 default of three recovery calls would spend quota silently.
            let config = CompactConfig {
                summary_max_requests: 1,
                ..CompactConfig::default()
            };
            let summary = tokio::select! {
                biased;
                _ = request.cancellation.cancelled() => None,
                result = peri_agent::agent::compact_v2::full::full_summary_only(
                    &transcript, Some(model.as_ref()), &config,
                ) => Some(result),
            };
            match summary {
                Some(Ok(summary)) => {
                    let flags = transcript
                        .entries()
                        .iter()
                        .filter(|entry| {
                            !transcript.flags(entry.id()).excluded
                                && !matches!(entry.as_message(), Some(BaseMessage::System { .. }))
                        })
                        .map(|entry| {
                            (
                                entry.id(),
                                MessageFlags {
                                    excluded: true,
                                    ..transcript.flags(entry.id())
                                },
                            )
                        })
                        .collect();
                    transcript.set_flags_batch(flags);
                    transcript.append(BaseMessage::human(format!(
                        "{}\n\n{}",
                        peri_agent::agent::compact_v2::CONTINUATION_HINT,
                        summary
                    )));
                    (TurnStatus::Completed, format!("上下文已压缩。\n可见消息：{} → {}\n继续在当前会话发送任务即可。原始历史已保留，摘要不会修改设备、目录、权限或模型选择。", before, transcript.visible_messages().len()))
                }
                Some(Err(error)) => {
                    tracing::warn!(error = %error, "manual compact failed");
                    (
                        TurnStatus::Failed,
                        "上下文压缩失败，原历史和上下文保持不变。请检查模型、额度或服务状态后再试。"
                            .to_owned(),
                    )
                }
                None => (
                    TurnStatus::Interrupted,
                    "上下文压缩已取消或达到时限，原历史和上下文保持不变。".to_owned(),
                ),
            }
        };
        // The normal journal settlement owns the atomic history/flags commit.
        // Never read cwd on the cloud host to re-inject remote project files.
        let history = transcript.canonical_payloads();
        let history_flags = history
            .iter()
            .map(|payload| (payload.id(), transcript.flags(payload.id())))
            .collect();
        let unsettled_jobs = self.bound_jobs().await.ok().map(|jobs| {
            jobs.into_iter()
                .filter(|job| {
                    !job.status.is_terminal() || job.status == JobStatus::RecoveryRequired
                })
                .collect()
        });
        CloudTurnResult {
            status,
            replies: vec![ChatReply {
                message_id: format!("compact:{}", request.turn_id),
                text,
            }],
            history,
            history_flags,
            internal_events: Vec::new(),
            unsettled_jobs,
        }
    }
}

pub(super) struct NoCommandEvents;
#[async_trait::async_trait]
impl peri_acp_types::event::EventSink for NoCommandEvents {
    async fn push_event(
        &self,
        _: &str,
        _: &peri_acp_types::event::ExecutorEvent,
        _: u32,
    ) -> Result<(), peri_acp_types::event::EventSinkError> {
        Ok(())
    }
    async fn push_done(&self, _: &str, _: &str, _: Option<&str>) {}
}

impl CloudAgent {
    pub(super) fn command_feedback(
        &self,
        request: CloudTurnRequest,
        status: TurnStatus,
        text: &str,
    ) -> CloudTurnResult {
        CloudTurnResult {
            status,
            replies: vec![ChatReply {
                message_id: format!("command:{}", request.turn_id),
                text: text.into(),
            }],
            history: request.history,
            history_flags: request.history_flags,
            internal_events: Vec::new(),
            unsettled_jobs: Some(Vec::new()),
        }
    }
    pub(super) fn history_command(
        &self,
        mut request: CloudTurnRequest,
        command: &peri_acp_types::command::ResolvedCommand,
    ) -> CloudTurnResult {
        let clear = command.entry.fullname == "core:clear";
        let start = if clear {
            0
        } else {
            let parsed = command
                .entry
                .args_schema
                .as_ref()
                .and_then(|schema| crate::commands::parse_arguments(schema, &command.args).ok())
                .expect("validated args");
            if !parsed.flags.iter().any(|flag| flag == "no-revert-files") {
                return self.command_feedback(request,TurnStatus::Failed,"当前执行器没有文件回滚快照，未执行回退。仅回退对话请使用 /rewind 消息ID --no-revert-files；不会在云主控本地执行 git checkout。");
            }
            let Some(index) = request
                .history
                .iter()
                .position(|p| p.id().as_uuid().to_string() == parsed.positionals[0])
            else {
                return self.command_feedback(
                    request,
                    TurnStatus::Failed,
                    "未找到该消息；使用 /history messages 查看本会话消息 ID。",
                );
            };
            if !matches!(
                request.history[index].as_message(),
                Some(BaseMessage::Human { .. })
            ) {
                return self.command_feedback(request,TurnStatus::Failed,"回退目标须是用户消息，避免拆开工具调用与结果。使用 /history messages 查看可用 ID。");
            }
            index
        };
        for (index, payload) in request.history.iter().enumerate() {
            if !matches!(payload.as_message(), Some(BaseMessage::System { .. })) {
                let mut flags = request
                    .history_flags
                    .get(&payload.id())
                    .cloned()
                    .unwrap_or_default();
                // Restore the canonical prefix, then hide the target and suffix.
                // Original payloads and past turn receipts remain untouched.
                if !clear && index < start {
                    flags = MessageFlags::default();
                } else {
                    flags.excluded = true;
                }
                request.history_flags.insert(payload.id(), flags);
            }
        }
        self.command_feedback(request,TurnStatus::Completed,if clear {
            "当前会话上下文已清空。设备、目录、权限、模型选择及原始历史保留。直接发送新任务即可。"
        } else { "上下文已回退至目标用户消息之前；目标及之后内容不再发送给模型。原始记录保留，未修改任何文件。" })
    }
    pub(super) fn handler_result(
        &self,
        mut request: CloudTurnRequest,
        result: peri_acp_types::command::CommandResult,
    ) -> CloudTurnResult {
        let old: HashMap<_, _> = request
            .history
            .iter()
            .filter_map(|p| p.as_message().map(|m| (m.id(), m)))
            .collect();
        let mut seen = HashSet::new();
        for message in &result.messages {
            if !seen.insert(message.id())
                || old.get(&message.id()).is_some_and(|old| {
                    serde_json::to_value(old).ok() != serde_json::to_value(message).ok()
                })
                || (!old.contains_key(&message.id())
                    && !matches!(message, BaseMessage::Human { .. } | BaseMessage::Ai { .. }))
            {
                return self.command_feedback(
                    request,
                    TurnStatus::Failed,
                    "命令返回了无效历史，本次未修改上下文。",
                );
            }
        }
        let positions: HashMap<_, _> = request
            .history
            .iter()
            .enumerate()
            .map(|(i, p)| (p.id(), i))
            .collect();
        let mut previous = None;
        let mut appended = false;
        for message in &result.messages {
            if let Some(index) = positions.get(&message.id()) {
                if appended || previous.is_some_and(|last| last >= *index) {
                    return self.command_feedback(
                        request,
                        TurnStatus::Failed,
                        "命令不能重排已有历史；本次未修改上下文。",
                    );
                }
                previous = Some(*index);
            } else {
                appended = true;
            }
        }
        let old_ids: HashSet<_> = old.keys().copied().collect();
        for id in old_ids.iter().filter(|id| !seen.contains(id)) {
            let flags = request.history_flags.entry(*id).or_default();
            flags.excluded = true;
        }
        request.history.extend(
            result
                .messages
                .into_iter()
                .filter(|m| !old_ids.contains(&m.id()))
                .map(PersistedPayload::Message),
        );
        let status = if result.stop_reason == peri_acp_types::command::PromptStopReason::Cancelled {
            TurnStatus::Interrupted
        } else if result
            .feedback
            .as_ref()
            .is_some_and(|f| f.level == peri_acp_types::command::FeedbackLevel::Error)
        {
            TurnStatus::Failed
        } else {
            TurnStatus::Completed
        };
        if let Some(feedback) = &result.feedback {
            if feedback.channel == peri_acp_types::command::FeedbackChannel::Session {
                request
                    .history
                    .push(PersistedPayload::Message(BaseMessage::human(format!(
                        "[Command feedback]\n{}",
                        feedback.message
                    ))));
            }
        }
        self.command_feedback(
            request,
            status,
            &result
                .feedback
                .map_or_else(|| "命令已完成。".into(), |f| f.message),
        )
    }
}
