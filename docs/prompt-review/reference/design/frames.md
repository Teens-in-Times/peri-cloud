<!-- PERI-EDIT-GUIDE:BEGIN -->
> **编辑说明（正式交付时清除）**
> - 功能：上游或历史设计参考。
> - 作用位置：未必启用，不能直接当作当前运行规则。
> - 应当写：批注希望借鉴的设计即可；启用与接入要另行确认，避免带入不存在的工具和路径。
> - 正文中可用 {{宏名}} 标记需求；未实现宏先作为设计标记。保留本段之外的正文作为创作内容。
<!-- PERI-EDIT-GUIDE:END -->

# 上下文帧与工具契约 · 讨论稿 v0.1

以下是未来宿主与模型之间的设计契约，不是当前已实现 API，也不是可直接提交给生产工具的参数。

## 建议的工具入口

- delegate_task：持久受理子任务，立即返回 task_id、受理状态和版本；不等待整个子任务完成。
- inspect_task：按 status / trace / conversation / artifact 视图读取；支持游标、范围和有界返回。
- message_task：发送补充、问题和约束变更；返回消息身份与接收进度。
- control_task：请求暂停、恢复或取消；返回控制收据，不把请求成功等同于外部操作已停止。
- annotate_task：可选语义注记。仅写发现、阻碍、依据和建议，不改运行时客观状态。

账号身份由宿主捕获；设备由可信目录解析。模型工具参数不包含 principal_id、运输令牌、SSH 凭据或 endpoint。控制和读取仍检查原任务、会话、设备与当前授权的关系。

## task_snapshot

owner / conversation_id / task_id / parent_task_id / worker_id / execution_session_id / turn_id / device_id / workspace_binding / task_revision / constraint_revision / observed_at 由宿主持有。

对模型披露必要字段：目标、约束、客观状态、最新活动、等待原因、当前有效的语义说明、产物与证据引用、信息时间和缺失说明。

客观状态至少区分 queued、running、waiting、completed、failed、cancel_requested、cancelled、outcome_unknown；waiting 的原因单独记录。具体状态机与允许迁移仍待实现设计。

## task_trace_delta

包含 task_id、from_cursor、to_cursor、observed_at、truncated、omitted_count 与有序条目。

条目可包括 tool_started / tool_finished、已经输出的行动说明、产物变更、约束变更及交互状态。每项关联 call_id / job_id / message_id 或 artifact_id；工具结果是原始观测，Agent 说明不冒充受信状态。

Read 默认仅返回路径、范围、结果大小、截取片段和原始引用。原文若未保留，返回 unavailable；需要当前内容时显式重新读取，形成新观测。

裁剪参数、最大条数、字数预算、保存期限和脱敏策略尚未确定。本页预览参数仅供设计，不声称是生产默认值。

## 周期进度帧

宿主统计已配置口径的 N 轮，将有变化的任务增量合并成一个有界帧，追加到主上下文尾部。没有变化时跳过。

一帧包含来源类型、事件 ID、任务版本、观察时间和游标。消费游标、去重与持久性由程序维护；普通 trace 不必启动新的主模型调用。

重要完成、失败和需要用户输入的事件可以独立入队。主会话串行消费事件，不能并发改写同一 transcript；用户消息具有交互优先级。具体仲裁与抑制规则待定。

## 控制收据

message_id、task_id、expected_revision、source、submitted_at 关联一次插话。

submitted / consumed / applied / rejected / unknown 含义不同；回复明确收据版本。约束变更有新 revision，并在派发新工具前检查。已经派发的操作保留原冻结绑定，不能通过改约束假装撤销。

## 记忆与缓存

长期记忆、任务快照、执行历史分别保存。记忆变更沿用现有 MemoryStore、CAS、privacy_epoch 与原审批；候选与后台整理尚未交付。

稳定规则与能力定义形成可复用前缀；任务与 trace 增量在尾部追加。阶段性压缩或隐私失效可重建视图；供应商缓存由 TTL、模型、设置、路由和实际前缀决定。

## 程序保证清单

持久派发、受理去重、单个云数据库所有者、前台容量保留、资源冲突协调、审批归属、可信设备绑定、进程退出检测、未知结果不重放、投送幂等和授权检查都由宿主负责。提示词仅描述期望行为。
