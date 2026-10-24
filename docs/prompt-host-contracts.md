# 提示词规则下沉：宿主契约

本页是工程与提示词创作说明，不发送给模型。运行角色文案见 `peri-cloud/prompts/`，可编辑合集见 `docs/prompt-review/PROMPTS-EDIT.md`。

## 已实现

| 领域 | 宿主保障 | 留给模型 |
| --- | --- | --- |
| 审批 | 原 PermissionMiddleware → WorkerBroker → 持久审批信箱 → 主会话队列；decide_approval 类型化参数和一次性提交 | 判断拟议动作是否符合当前用户意图 |
| 简单操作 | 原生读取及严格字面量输出/位置命令由有界宿主策略处理；审批记录区分 host_read_only 与 main_agent | 对未命中规则的动作权衡必要性、范围与误解 |
| 删除 | 原生工具语义及保守 Shell 分析；不透明操作按可能删除；原 outbox 通知确认后放行 | 可补充风险，不能降低宿主判定 |
| 摘要 | 初始与恢复请求共用空工具构造入口；工具形状响应拒绝；预算、有界恢复和原子历史替换 | 提取关键事实与决定压缩取舍 |
| 换窗 | 完整活跃快照、持久换窗意图、独立无工具接班请求、原子提交及启动前重核；手动无关联活跃主 goal 时就绪，未知摘要不重试 | 解释关键发现与下一步，按需检索证据 |
| 幂等 | 持久 session/turn/call 身份与独立参数指纹；Dispatch/Existing 类型分支；原回执缓存或原调用查询 | 判断下一步，以及是否确实需要一个新的动作 |
| 渠道 | CommonMark 解析转 QQ 纯文本，表格/链接/列表/代码专门处理；工具详情与菜单保持独立 | 自然表达、语气与详略 |
| Skill | Rust 合并优先级；默认生效目录、稳定 Skill ID；可显式查看遮蔽条目，正文按需读取 | 选择适用的专业指导 |

## 主 agent 审批的运行方式

不再创建独立空历史审查 Session，也不解析 assistant 普通文本中的审批 JSON。请求进入原主会话的持久双向事件队列，当前聊天可在模型边界看到它；空闲时用同一主会话历史和 Agent loop 处理控制事件。

控制事件每次最多三次迭代、五秒推理时间，只暴露 decide_approval、inspect_task、message_task。决定成功后直接结束控制回合，控制回合文字不作为普通聊天投递。普通聊天与控制事件经原会话串行门和 FIFO 排队，不并发改写历史。一个审批请求最多触发三次控制事件，受原日回合与模型额度限制；原请求有效期仍为九十秒。

`agent_task_approval_presentations`（schema 24）绑定审批请求、实际主会话轮次和精确请求指纹。只有本轮收到完整参数的主 agent 可提交决定；未呈现、越权、过期、重复、取消或旧指令版本均不能授权。普通双向文字消息不会被解析为批准。最终执行仍由原 PermissionMiddleware、审批消费和原生调用受理检查决定。

等待主 agent 和删除通知时，执行者让出后台额度。主会话控制回合有界但仍占用自己的串行推理时间；同一会话不承诺在一个未结束的模型请求里同时回复另一条聊天。

独占宿主重启时，可信控制回合只有在原生操作账本确认没有未结算调用的情况下才直接结束为 interrupted；不重放审批或双向消息，其实际结果由持久账本核对。普通执行回合与任何未知原生操作继续走原恢复流程。这样控制回合中途崩溃不会把主会话长期留在执行未知状态。

## 活跃约束快照

保留全部父级补充原文与顺序，不再采用最近八条、每条两百字预览作为启动材料。宿主注入完整快照后登记窗口呈现状态；这不是模型已经理解或落实要求的声明。正常启动和换窗不需要先 inspect_task 再 update_task。

模型推理期间新增要求会使旧批次被阻断；宿主下一轮刷新快照。派发检查点只使用已呈现消息的水位，并在事务内对照最新水位，避免并发补充被错误当作已经看过。

完整要求超出预留容量时保存 waiting_external 与明确原因并通知主助手，保留原任务，不静默裁剪，不循环空换窗。第一版不自动推断自然语言要求的替代关系，不做语义契约折叠。

## 幂等范围

稳定键沿用宿主持久化的来源会话、轮次和调用 ID，设备、绑定、工具及参数单独做指纹校验。同键异参拒绝；同键已确认返回原回执；同键未确认只查询原操作，无可用回执则继续保留未知状态。设备互斥与未知操作占用保持原语义。换窗只引用原操作，不为其创建新身份。

这不把相同参数的不同调用自动合并；例如两次用户要求的相同发送可以是两个动作。任意换 ID 的语义重复、外部工具缺少查询能力时的恰好一次投递，不属于当前保证。

## 安全分析与渲染边界

Shell 第一版采用保守的受限字面量分析，并非通用 Bash/PowerShell AST 解释器。脚本、解释器、动态展开和未知工具不会因模型自称安全而进入快速允许分支。未知风险按可能删除通知；不增加人工确认门槛，也不扩大账号或设备授权。

QQ 渲染只改变 AI 回复的展示，不改变规范历史、完整工具参数或验证证据。代码正文和 URL 保留；OneBot 仍发送文本 segment。已有不确认投递不重试，Markdown 转换不授予重发权。

## 验证

已通过八项云端库针对性检查（完整长要求及并发补充水位、删除分析、QQ 渲染、生效 Skill 目录与按需读取），以及十九项摘要续写与失败保留检查。十三项执行链联调也已全部通过，覆盖审批、旧要求失效、通知未知、重复调用、跨设备固定目标、并行与串行、低预算换窗、原回执查询及真实进程强杀恢复。合计四十项针对性检查，使用模拟模型和本地执行器，没有运行全仓测试。

cargo fmt --all --check 通过；peri-cloud、peri-agent、peri-executor、peri-remote-tools、peri-acp-types 的 all-targets 严格 Clippy 通过。构建均通过 scripts/cargo-rmcp-patched.sh 应用固定补丁。上游清单校验为 1291 个文件、1261 个一致、30 个已声明生产补丁，无未声明漂移；变更 Rust 源码与测试均少于 1000 行。

复现主要检查：

- 云库：cargo-rmcp-patched.sh test --locked -p peri-cloud --lib -- snapshot_tests gateway::plain workspace::skills tasks::deletion
- 摘要：cargo-rmcp-patched.sh test --locked -p peri-agent --lib continuation_tests
- 执行链：cargo-rmcp-patched.sh test --locked -p peri-cloud --test gateway -- approval_tests deletion_tests approval_capacity_tests duplicate_native_call low_budget per_call_target process_kill_preserves_wait recovery_queries_original_receipt independent_execution_sessions --test-threads 3

换窗的请求预算、输出上限、摘要失败降级和历史工具契约见 [schema 26 窗口运行契约](worker-window-runtime.md)。提示词合集保留创作指引与装配说明，工程验证记录放在独立文档中。
