# Grok Build / Grok Bot 调研与 Peri 可借鉴设计

调研日期：2026-10-18。本文是研究建议，不代表这些能力已经在 Peri 实现。

核心判断：优先借鉴记忆的提取与整理流水线、统一唤醒入口和联系策略。现有独立调度 crate、原 Agent 循环、权限中间件、SQLite 记忆权威存储与 MessageAdapter 可以承接这些能力。

Grok Bot 的整体个人助手设计、Action 运行流程、显式通信和状态归属另见 [Grok Bot 整体架构专题](grok-bot-personal-agent-architecture-20261018.md)。

## 1. 研究对象与证据边界

| 对象 | 本次使用的证据 | 能说明什么 |
| --- | --- | --- |
| Grok Build | 官方仓库，固定到 `2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8` | CLI/TUI 与 Agent runtime 的公开实现；不是完整模型与云服务源码 |
| Grok Bot 0.18.0 | 社区安装包运行时拆分档案，固定到 `bd06ef2cab1becec4b78d5237790a6783737460b` | 档案中可读的早期客户端/运行时代码；不能证明当前云端调度、投送和记忆服务的完整实现 |
| 当前 Grok Bot | 官方产品文档，主要页面更新于 2026-10-12 | 当前公开的产品行为与概念，不等于底层代码实现证据 |

Build 官方仓库声明第一方代码为 Apache-2.0，并要求保留第三方许可与归属。Bot 档案明确声明自己不授予代码使用权；本次只研究机制，没有复制其运行时代码进入 Peri。[Build 官方说明][build-readme]、[Bot 档案说明][bot-readme]

Bot 档案作者称，原安装包携带未压缩的 Electron 运行时 bundle，按 esbuild 模块标记拆分即可得到可读模块；不是通过 source map 恢复完整工程。本文读取了档案模块及其验证说明，但没有独立下载官方安装包重做字节与哈希链验证。“被迫开源”的动机与因果关系未在本次核实。[验证步骤][bot-verification]

另一个 reconstructed 仓库增加了推理路由、本地 Docker 和用量统计等功能，这些不能作为官方 0.18.0 能力的证据。[新增内容说明][bot-additions]

## 2. Grok Build：最值得研究的是记忆流水线

### 2.1 提取和整理分开

运行时 capture 模块明确采用无工具调用的提取：模型只收到固定的已持久化对话快照与严格 JSON schema，宿主校验后提交。提取具有独立任务状态与提示词版本。[capture 运行时][build-capture-runtime]

底层 capture 存储保存不可变 observation，并分别处理“文件已发布但结果未提交”和“结果已提交但索引游标未推进”的崩溃窗口。[capture 存储][build-capture-store]

整理模块另行领取固定 observation 集合；模型工作不占用数据库事务。整理计划包含创建、更新、合并、拆分等操作及其证据，提交前验证租约代际，支持确定性恢复。[consolidation][build-consolidation]

对 Peri 的建议是两条明确的写入路径：

```text
用户明确要求记住 / 更正 / 忘记
  → 现有记忆工具、原权限中间件与 MemoryStore

符合采集授权的已完成回合
  → 当前授权、隐私截止点过滤后的固定快照
  → 无工具的结构化提取
  → 持久候选箱
  → 去重 / 冲突识别 / 整理建议
  → 现有记忆服务与审批规则
  → MemoryStore
```

提取器不能拥有设备执行、QQ 发送或任意记忆写权限。候选不是事实；模型的推断不能借后台整理自动变成用户已经确认的个人资料。

### 2.2 学并发控制，保留一个权威存储

Peri 已有 SQLite 事实、版本校验、删除标记、索引和审计的事务边界。可以借鉴 capture 游标、证据关联、租约代际和整理计划，但没有必要照搬 Markdown 文件、状态数据库与索引之间的多阶段协调。[现有记忆实现说明](../memory.md)

建议候选至少带上：

| 字段 | 用途 |
| --- | --- |
| `owner_id`、授权 scope 与绑定身份 | 从宿主真实授权获得，不能由提取模型自行选择其他账号或设备 |
| `source_session_id`、来源回合/消息 ID 与采集游标 | 可追溯、增量采集与幂等 |
| `captured_privacy_epoch`、基础记忆版本 | 防止忘记、更正和撤权后，旧后台结果再次写入 |
| 提取模型、提示词版本、证据与生命周期 | 能解释来源，能重做派生结果，区分临时状态与长期偏好 |

生成完成后与晋升提交前都重新验证授权、隐私版本和基础事实版本。任何来源已失效的候选应作废或重新采集，不能只给旧候选换一个新版本号继续提交。

### 2.3 遗忘必须覆盖后台任务

Build v2 maintenance 先持久化 tombstone，再清理文件；读者在崩溃恢复期间也必须以删除账本为准。forget 还要求调用方提供先前查看内容的哈希。[maintenance][build-maintenance]

Peri 已有 `privacy_epoch` 和旧模型视图截止点。增加后台采集后，遗忘还应使旧候选、未提交整理计划与派生索引失效。否则用户刚说“忘记”，后台任务可能从旧对话中再次提取同一信息。

不宜据此断言哪个产品的遗忘整体更强：文件删除、未来检索、当前模型上下文和已发送供应商的数据，是不同边界。

### 2.4 记忆是带来源的历史，不是永远正确的指令

Build 的记忆注入明确要求：将记忆视作历史上下文；路径、命令、仓库状态与外部事实要重新验证，冲突时优先当前证据。[上下文注入][build-memory-context]

Peri 可以沿用这一原则：长期偏好、近期状态、设备环境和任务方法分别有不同生命周期。“今天心情不好”不应永久晋升为性格标签；“昨天设备在线”不能证明现在可执行。当前少量固定记忆加 FTS 的方案可以继续使用，尚无证据说明必须先引入向量数据库。[Build 检索配置][build-memory-guide]

## 3. Grok Bot：让长期助手连贯运行

### 3.1 通知驱动同一套 Agent 处理

早期档案中的 `SubscriptionNotificationActionHandler` 将通知转换成消息处理输入，显式标记 `isSyntheticWakeup`，随后委托普通 `userMessageActionHandler` 的处理方法。[通知处理模块][bot-wakeup-handler]

普通消息处理器也检查这个标记：系统唤醒不会像真实用户新消息一样恢复被暂停的 goal。这个分支说明“复用处理流程”与“保留来源语义”同时成立。[普通消息处理器][bot-user-handler]

这说明在被读取的这条路径中，主动行为通过事件唤醒已有处理流程完成。不能据此推导其整个云服务架构。

“事件与对话分离”可以表达方向，更准确的说法是触发、会话、执行与投送分层：

| 对象 | 回答的问题 | Peri 中应携带或管理的内容 |
| --- | --- | --- |
| 事件 / 唤醒 | 为什么现在需要处理？ | 来源、事件身份、真实 owner、目标会话、时间或外部证据 |
| 会话 | 在哪个连续语境中处理？ | 对话历史、摘要、可见性、上下文代次与当前授权记忆 |
| 运行 / 回合 | 本次做了什么，处于什么状态？ | 入队、执行、工具审批、取消、结果与失败 |
| 投送 | 是否应该打扰用户，发到哪里，结果如何？ | 联系策略、当前路由、持久发件箱与平台接受状态 |

这是用于 Peri 的设计解释，不是对 Grok 所有内部类型与数据库表的还原。事件引用会话，不必携带或复制整段历史；一次唤醒可以产生一次运行，运行也可以静默结束。系统唤醒不能因此取得恢复用户暂停任务或批准工具的权限。

Peri 可采用以下分工：

```text
时间触发 ── peri-scheduler ─┐
外部事件 ── 事件接入适配器 ─┼→ 云端可信唤醒入口 → 原 Runtime / Agent / 权限中间件
普通聊天 ── MessageAdapter ─┘                         ↓
                                             联系策略与持久发件箱
                                                     ↓
                                               MessageAdapter
```

未来的统一入口携带 owner、原会话/路由、事件 ID、上下文代次和任务版本；事件内容仍是不可信输入，不能伪装成人类批准。调度库只生产时间事件，无需接管账号、记忆或全部外部事件订阅。

### 3.2 合并唤醒，但可靠性继续由持久层负责

档案的 `InMemoryLocalWakeupQueue` 有按会话与 ID 去重、合并、领取、确认、退回和抑制逻辑，适合研究通知突发时的控制。合并函数按 `conversationId + groupKey` 替换同组待处理唤醒，独立于聊天文本格式。它明确是内存队列，不能作为持久 cron 或可靠投送的证据。[后台工作队列][bot-wakeup-queue]、[合并函数][bot-wakeup-merge]

Peri 可学习同一会话的唤醒合并与背压；仍由现有 occurrence、Runtime 入队去重和 outbox 管理持久结果。对执行或发送结果未知的记录继续禁止自动重放。[现有调度实现说明](../scheduler.md)

Build 的已读 scheduler actor 则明确写出每次触发通过子 Agent 运行，其启动过程要等待子 Agent 接线完成。这是运行时内部的定时派发设计，不能直接称为独立云端调度服务。Peri 继续采用调度 crate 生产 occurrence、云适配层提交原 Runtime 的边界。[Build scheduler actor][build-scheduler-actor]

### 3.3 Skill 负责方法，Routine 负责触发

官方文档将 Skill 定义为可复用的方法，Routine 定义为某个 Bot 何时执行工作流，支持时间或特定事件。它建议先完成一次任务、稳定方法，再自动化；测试运行会执行真实工作。[Skills / routines][bot-routines]

对 Peri 可以进一步区分：

| 概念 | 放什么 | 示例 |
| --- | --- | --- |
| 助手设定 | 用户选择的称呼、语气与角色 | 温柔、简短，偶尔使用表情 |
| 记忆 | 有来源、可更正与遗忘的事实或偏好 | 用户喜欢下午收到一句轻问候 |
| Skill | 经验证的方法、输入、输出与检查步骤 | 每周回顾最近任务并生成摘要 |
| Routine / 计划 | 时间/事件、任务引用、目标、联系策略 | 工作日下午检查是否适合问候 |

这些对象通过版本引用连接；不能把 Skill 正文当成权限授予，也不能让每次修改方法悄悄改变一个已批准计划的含义。

### 3.4 上下文预算与压缩恢复

Bot 早期代码把初始 Skill 目录预算设为模型上下文窗口估算的 2%；超限时逐步缩短描述、仅留路径，最后省略条目并提示省略情况。[Skill 目录预算][bot-skill-budget]

另一个模块会从最后的真实用户消息中提取手动附加的 Skill 块，供摘要过程保留。这不是自动学习个人事实的实现。[Skill 保留][bot-skill-persistence]

可借鉴预算分配和按需加载：目录给模型发现能力，当前执行的 Skill 才加载正文；压缩后重建仍有效的结构化上下文。Peri 已有记忆帧重发机制，后续可以扩展这一机制，不必把所有记忆与所有 Skill 永久塞进提示词。

### 3.5 主动联系应管理用户注意力

当前官方文档区分等待用户处理、新结果未读和正在工作；通知通常面向完成或需要输入，并在应用聚焦时抑制系统通知。[通知设计][bot-notifications]

Peri 已有 `when_useful`、联系窗口、总开关和每日后台额度。下一步建议增加会话冷却、近期实际互动、同类事件合并以及明确的发送/静默/延期决策。QQ 没有可靠已读证据时，不能把平台接受发送推断成用户已读。

对于用户授权的下午问候：到点后检查是否刚聊过、今天是否已问候、是否在联系窗口；合适时生成一句自然的问候，用户不回复就结束，不逐次升级催促。语气与稳定偏好来自当前授权记忆，不能借问候主动暴露无关私人事实。

档案还对下游子代理排除了部分平台通信工具，可借鉴其“工作者返回结果，由上层统一对外发送”的分工。Peri 继续保持 MessageAdapter 与持久发件箱作为消息出口。[通信工具分工][bot-communication]

## 4. 一个具体的反面经验：工具审批不覆盖所有数据出站

研究者公开了针对 Build `0.2.93`（2026-07）的网络复现材料，报告完整 tracked repository 与 Git 历史的上传独立于模型读文件行为。作者明确限定：证据说明传输与存储，不证明训练，而且行为与版本相关。本次未在本机重跑该实验。[原始复现材料][build-privacy-repro]

当前官方文档则说明 Build CLI 的 ZDR 与 `/privacy` 数据保留控制。历史复现不能直接代表今天版本或所有设置。[当前官方说明][build-privacy-current]

对 Peri 的直接启发是：诊断、遥测、导出与备份的出站路径也应遵守各自授权；不能因为工具读文件需要审批，就假设后台打包和上传天然受到相同限制。可用合成 canary 数据验证这些路径，不使用个人真实历史作为测试材料。

## 5. 建议落地顺序

| 顺序 | 工作 | 完成标准 |
| --- | --- | --- |
| P0 | 记忆采集任务、候选箱、来源游标 | 仅采集授权且当前可见的来源；结构化提取无工具；重复任务不重复写候选 |
| P0 | 候选晋升与隐私失效 | 更正、忘记、撤权后，旧后台结果不能再次晋升；提交继续通过现有服务和审批 |
| P1 | 候选去重、冲突识别、整理计划 | 有证据、有基础版本；并发更正不被覆盖；崩溃后结果明确 |
| P1 | 主动联系冷却与注意力策略 | 发送/静默/延期可解释；不追问轰炸；保持未知发送不重试 |
| P2 | Skill 版本与按需加载 | 方法与事实分离；上下文压缩后保留有效方法；计划版本变化可审阅 |
| P2 | 外部事件唤醒适配 | 校验真实 owner 与来源，合并事件，仍走原 Runtime；不扩大调度库职责 |

不建议为了借鉴而引入新的 Agent 主循环、TUI 依赖、多套记忆权威存储或模型自批权限。Build 的本地 global/workspace 范围不能直接代替 Peri 多账号、设备和项目的授权；其会话内调度也不能代替我们持久的一次性计划与未知结果处理。

本次仅新增本文；生产代码、依赖与运行服务未因调研改变。

[build-readme]: https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/README.md
[bot-readme]: https://github.com/ChHsiching/grok-bot-0.18-original/blob/bd06ef2cab1becec4b78d5237790a6783737460b/README.md
[bot-verification]: https://github.com/ChHsiching/grok-bot-0.18-original/blob/bd06ef2cab1becec4b78d5237790a6783737460b/VERIFICATION.md
[bot-additions]: https://github.com/ChHsiching/grok-bot-0.18-original/blob/bd06ef2cab1becec4b78d5237790a6783737460b/BN-NETT-ADDITIONS.md
[build-capture-runtime]: https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/session/memory/v2_capture.rs#L1-L5
[build-capture-store]: https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-memory/src/v2_capture.rs#L1-L8
[build-consolidation]: https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-memory/src/v2_consolidation.rs#L1-L6
[build-maintenance]: https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-memory/src/v2_maintenance.rs#L1-L5
[build-memory-context]: https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-shell/src/session/helpers/memory_context.rs#L31-L36
[build-memory-guide]: https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-pager/docs/user-guide/13-memory.md
[bot-wakeup-handler]: https://github.com/ChHsiching/grok-bot-0.18-original/blob/bd06ef2cab1becec4b78d5237790a6783737460b/split/dist/host/host-main.cjs/packages/agent/dist/actions/subscription-notification-action-handler.js#L2-L36
[bot-user-handler]: https://github.com/ChHsiching/grok-bot-0.18-original/blob/bd06ef2cab1becec4b78d5237790a6783737460b/split/dist/host/host-main.cjs/packages/agent/dist/actions/user-message-action/user-message-action-handler.js--2#L809-L828
[bot-wakeup-queue]: https://github.com/ChHsiching/grok-bot-0.18-original/blob/bd06ef2cab1becec4b78d5237790a6783737460b/split/dist/host/host-main.cjs/packages/agent-exec/dist/background-work-registry.js#L17-L122
[bot-wakeup-merge]: https://github.com/ChHsiching/grok-bot-0.18-original/blob/bd06ef2cab1becec4b78d5237790a6783737460b/split/dist/host/host-main.cjs/packages/agent-exec/dist/wakeup/index.js#L2-L17
[build-scheduler-actor]: https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-tools/src/implementations/grok_build/scheduler/actor.rs#L244-L254
[bot-routines]: https://docs.x.ai/grok-bot/skills-routines-and-automations
[bot-skill-budget]: https://github.com/ChHsiching/grok-bot-0.18-original/blob/bd06ef2cab1becec4b78d5237790a6783737460b/split/dist/host/host-main.cjs/packages/agent/dist/prompts/skill-catalog-budget.js#L1-L73
[bot-skill-persistence]: https://github.com/ChHsiching/grok-bot-0.18-original/blob/bd06ef2cab1becec4b78d5237790a6783737460b/split/dist/host/host-main.cjs/packages/agent-summarization/dist/skill-persistence.js
[bot-notifications]: https://docs.x.ai/grok-bot/settings-and-notifications
[bot-communication]: https://github.com/ChHsiching/grok-bot-0.18-original/blob/bd06ef2cab1becec4b78d5237790a6783737460b/split/dist/host/host-main.cjs/packages/agent/dist/automations/platform-communication-tools.js
[build-privacy-repro]: https://github.com/cereblab/grok-build-exfil-repro#ethics--scope
[build-privacy-current]: https://docs.x.ai/developers/faq/security#grok-build-cli
