# Agent 核心重构：运行契约与落地状态

状态：核心实现完成，本地真实宿主与执行器联调通过。原设计工作台改动保留；验证范围见文末。

## 目标和范围

同一个可信账号可以在不同消息网关显式选择同一逻辑会话，也可以新建独立会话。渠道路由、逻辑会话、每轮回复来源分别保存。普通新消息进入同一会话的持久 FIFO，当前轮次结束后再受理；审批、取消、目录选择等仍属于原控制面。

上下文使用 `ContextAssembler`，组合基础提示词、角色、SOUL、渠道约束、原记忆协议、外置 Markdown 和原生工具。配置的段落顺序不改变权限中间件顺序；原记忆帧和已有工作区中间件仍按自己的授权、刷新和预算契约提供动态内容。本次不实现新记忆提炼、MCP、Skill 或自我进化业务。

主 Agent 和执行 Agent 都运行在云宿主。主 Agent 保留原生工具和 Todo，短操作直接执行；委派工具持久受理后立即返回。执行 Agent 有独立冻结会话、历史、设备/工作区和角色，使用同一装配器与原推理循环，保留原工具审批。

## 已接入的接口

- `POST /api/web/chat/conversation`：`request_id` 与可选 `session_id`；省略目标即新建，提供目标即选择账号有权访问的历史。重复请求返回原选择结果，不强制重新选择当前入口。
- `POST /api/web/chat/messages`：支持 `request_id`，原身份与原文复核后去重。
- `GET/POST /api/context/files`：已认证账号读取、管理自己的 SOUL.md；写入沿用 Cookie、同源与 CSRF 校验。账号目录由可信 principal 生成，不接受 owner 或任意路径。
- QQ / OneBot 保留 `/new` 和 `/resume 会话ID`。跨入口恢复不替换进行中轮次的 broker，后续输入在边界重新绑定。
- 主 Agent 工具：`delegate_task`、`list_tasks`、`inspect_task`、`message_task`、`control_task`。
- 执行 Agent 工具：`inspect_task`、`update_task`、`message_parent`。问题可用 `wait_for_reply` 异步等待主助手；主助手仍可继续聊天。

任务的 `state` 来自宿主生命周期；`note`、`work_plan` 与 `result` 是带来源的 Agent 声明。执行完成不冒充独立验收通过。插话区分 submitted、consumed、applied，取消请求不等同于副作用撤销。

## 配置与文件

host.json 可增加 `context`：

```json
{
  "directory": "./context",
  "order": ["base", "role", "soul", "channel", "memory_protocol", "files", "tools"],
  "files": [],
  "max_bytes": 65536
}
```

目录相对 host.json 解析；未指定时使用状态目录下的 context。全局 `SOUL.md` 和显式配置的 Markdown 属于部署者；用户文件位于 `users/<principal>/SOUL.md`。用户文件只在私人入口装配。符号链接跨用户、目录逃逸、非法编码和超预算内容失败关闭。每轮读取一次，写入从后续轮次生效。

前台沿用 `max_active_turns` / `max_active_per_user`；后台使用独立 `max_background_turns` / `max_background_per_user`，默认 2 / 1。请求额度与时限继续使用现有机制。

## 持久性与恢复

schema 16 增加会话消息 FIFO、任务账本、双向消息、trace 预览与设备修改占用。单一 CloudJournal 仍是数据库所有者。

入队保存目标 session 和原始回复引用；切换入口或选择其他会话不改变已经受理的目标。只有原云轮次不存在且唯一旧宿主已退出，才可以继续派发尚未受理的队列项；已经受理、执行未知或发送未知保留原身份，不另建任务重做。

后台完成消息是带类型的任务事件，作为规范提醒进入主会话，不伪装成人类输入。普通聊天仍只投送主 Agent 的 AI 正文。trace 记录工具调用、裁剪结果和可见行动说明，排除模型内部思考；详细查看尊重原历史排除标记与当前隐私版本。

首版按设备串行协调修改。执行任务预占设备，主助手仍能聊天；同设备的修改需等待任务结算或通过任务消息协调。占用由宿主执行协调维护，与工具审批分开。存在未确认执行时不释放占用。

## 验证记录（2026-10-19，Windows）

后续执行连续性重构正在实施，新增会话复用与 schema 17 的当前进展见 [执行连续性计划](execution-continuity-plan.md)。下列验证记录针对此前首版，不能作为窗口轮换、自动审批或持久恢复已经交付的证据。

使用模拟模型和隔离的真实 `peri-cloud-host`、本地执行器进行联调，未连接个人账号或线上模型。通过的流程包括：

- 同一请求重复发送只受理一次；显式新建独立会话为空白，切回原会话保留共享聊天投影。
- 主助手委派后，执行 Agent 的原生 Write 由其真实 session/turn 请求审批；批准前文件不存在，批准后实际产生文件。
- 执行 Agent 通过 message_parent 异步等待，主助手继续聊天并读取 live 会话记录；message_task 唤醒执行 Agent，消息最终标记 applied。
- 宿主保存任务状态、工具 trace、最终结果，主会话生成普通 AI 回复；内部思考未出现在聊天或 trace，结算后释放设备修改占用。
- 全局 SOUL、账号 SOUL、外置 Markdown 同时进入主/执行上下文；账号 SOUL 可创建和覆盖保存。
- 阻塞当前模型调用时，后续两条输入返回 queued；释放后按持久 FIFO 顺序处理。
- 待审批任务取消后没有写入文件；清空主会话使旧工作者失效、待审批请求关闭，旧结果不回流。
- 服务正常关闭后重新启动，之前排队的输入继续处理；原始输入各对应一个持久受理记录，没有重新派发旧执行。

README 指定的六个 crate 现有检查共 476 项通过；原 OAuth、QQ/OneBot、隐私、权限与未知执行/投送检查均继续运行。没有新增测试用例或测试文件，只同步了七处现有迁移夹具、版本断言和 `/new` 行为断言。

检查命令使用 `scripts/cargo-rmcp-patched.sh`，包含目标测试、`cargo fmt` 和 peri-cloud 的所有 target 严格 Clippy；Windows 内存限制下使用 `-j 1`。门户 JavaScript 语法与 source/test 文件行数边界也已检查。

本轮验证使用 Windows 与模拟外部服务；没有进行真实 QQ 平台、线上模型或 Linux 现场部署验收。首版仍按整台设备协调修改，后台任务的进程崩溃沿用原恢复语义：保留未知执行，不自动重跑。
