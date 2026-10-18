# QQ 原生命令接口

QQ 使用原生 Slash 命令，不把管理命令交给模型猜测。官方 QQBot 与个人 QQ/OneBot 共用 Gateway；中文命令、原菜单及编号入口保留。Web 可调用同一套认证后端接口，不安装 TUI 或另建 Agent。

## 日常操作

| 命令 | 行为 |
| --- | --- |
| `/new`、`/新会话` | 在当前设备及同一目录新建空白会话，继承权限和显式模型选择，保留旧历史；不调用模型 |
| `/compact`、`/compress`、`/压缩`、`/core:compact` | 使用当前模型的原生 Full Compact 摘要生成逻辑，生成摘要后继续当前会话；原始记录保留，消耗模型请求额度 |
| `/clear`、`/cls`、`/reset`、`/core:clear` | 清空当前可见上下文，保留同一会话、设备、目录、权限、模型和原始记录；不调用模型 |
| `/rewind 消息ID --no-revert-files`、`/undo ...` | 回退上下文到该用户消息之前；不修改文件。原始消息和历史任务回执保留 |
| `/loop 目标`、`/core:loop 目标` | 使用原生目标注入语义进入 Agent 循环，仍受现有迭代、时限、并发、额度及工具审批限制 |
| `/mcp list`、`/mcp add <名称> <https://...>`、`/mcp remove <名称>`、`/mcp enable\|disable <名称>` | 管理本账号的 MCP 服务。只接受公网 HTTPS 端点（拒绝本机/内网/保留地址、URL 凭据和查询参数），不保存凭据；MCP 工具名为 `mcp__<名称>__*`，调用仍走审批。配置在下一轮对话生效 |
| `/history [页码]`、`/threads`、`/his` | 分页列出本账号历史会话 |
| `/history messages [页码]` | 查看当前会话用户消息 ID，供 rewind 使用 |
| `/resume 会话ID` | 验证账号、设备和空闲状态后恢复历史会话，沿用该会话保存的权限与模型；不重新运行工具 |
| `/pwd`、`/context` | 当前会话、设备、目录和权限 |
| `/status`、`/tasks` | 当前任务状态 |
| `/cancel`、`/stop` | 请求取消当前任务；反馈不等于执行器已停止 |
| `/result [页码]` | 查看已保存结果 |
| `/devices`、`/connect 设备ID [绝对路径]` | 选择或连接设备 |
| `/workspace [绝对路径]` | 目录选择或切换，路径含空格会保留 |
| `/model [模型名]` | 模型菜单或选择；`default` 恢复默认 |
| `/permissions [default\|edit\|all]` | 权限菜单或选择默认/编辑/全部 |
| `/usage`、`/memory`、`/settings` | 原用量、记忆和个人设置服务 |
| `/gateway` | 管理员私聊管理入口白黑名单 |
| `/menu` | 原操作菜单 |
| `/help`、`/commands`、`/命令` | 用法；`/help all [页码]` 查看全部命令及接入状态，`/help core:rewind` 查看单条说明 |

/new、compact、clear、rewind、resume 均不会强行覆盖运行中的任务、等待审批或未确认的执行状态。先等待任务结算，或取消后检查状态。历史会话若仍被另一聊天路由选中，不能跨入口重绑其审批和投递目标。

QQ 命令需要是平台实际投递的文字消息。官方机器人若把 `/...` 当成平台自己的命令选择器，应选中机器人并按其入口发送；Peri 无法处理平台未投递的文字。按钮菜单仍可作为入口。

## 原生 CLI 的真实兼容范围

源项目的 Agent 核心注册了 `core:compact`、`core:clear`、`core:rewind`、`core:loop`；`/new` 与 `/mcp` 是本云端新增的会话与账号 MCP 控制入口。底层继续使用 `peri_acp_types::command::{CommandRegistry, CommandHandler, CommandOutcome, ArgsSchema}`，支持完整名称、别名和参数校验。

CloudJournal 是云端历史的权威：摘要生成复用原生 Agent 实现，历史与 flags 在原轮次 settlement 的 SQLite 事务一起提交，不给远程路径套上云主机本地的 SessionResources。compact 不从云主控 cwd 重新读取文件或 Skill，不运行工具。clear/rewind 保留 canonical 原文，通过排除标志改变下一次模型输入；旧任务结果不改写。

文件回滚尚不支持：原生 rewind 的默认实现会在本机 cwd 执行文件恢复；直接照搬到云端会操作错误机器。未传 `--no-revert-files` 时明确失败，不运行 `git checkout`，也不降级成未声明的纯对话回退。回退目标限定用户消息，避免拆开工具调用和结果。

源项目的 `ui:` 命令是 TUI 的本地面板，不是可在 Agent 中直接执行的 Shell：

- 已有云端对应能力的 model、login、config、threads/history/resume/his、status、memory、tasks、setup 映射到现有 QQ 控制或结构化 Web 页面。
- agent、hooks、plugin、cron-list、betas、workflows 的云管理服务目前没有装配；全部名称在目录中公开，执行返回 unavailable/unsupported 及原因。MCP 的云端子集由 `/mcp` 提供（账号级 HTTPS 服务），`ui:mcp` TUI 面板仍不可用。
- theme 属于本地终端显示，QQ 无此渲染能力。

这些不可用条目不是成功的占位实现，不能把“接口已经开放”表述为全部 TUI 后台能力已经移植。动态 Skill/MCP 命令只有在相应服务实际发现并登记 handler 后才可执行，不任意透传未知 slash 文本到模型。

## 正式发现和执行 API

`GET /api/commands`：浏览器认证后返回全部条目：name、aliases、description、args、availability（available/limited/adapted/unavailable）及 reason。查询不调用模型。管理操作不是任意执行服务器 Shell 的入口。

`POST /api/conversations/{session_id}/commands`：浏览器 Cookie、同源、CSRF、资源归属和入口名单校验；所有请求须 confirmed=true。请求示例：

```json
{
  "request_id": "11111111-1111-4111-8111-111111111111",
  "command": "/compact",
  "confirmed": true
}
```

耗时原生命令通过已有 Runtime 接纳，返回 `state: accepted`、task.turn_id；从原会话状态、结果、待办 API 继续查询或取消。HTTP 断开不丢弃任务。相同请求 ID 和内容不重复执行；冲突内容拒绝。new/resume 等短控制使用现有 control_operations 表保存请求状态和结果；结果未知不自动重放。new 的重复请求返回原新会话 ID，即使原会话已不再选中。

返回 completed 表示控制操作完成；interactive 表示需要在现有结构化面板继续；unsupported 明确没有执行。API 的交互面板入口不接受明文 Key 配置参数，凭据仍走已有专用模型接口。QQ 可以用原命令完成相应选择。

## 为更多原生命令注册执行者

服务器组合根可使用 `NativeSessionConnector::register_command(RouteEntry)`，接口沿用原生 CommandHandler；不是上传脚本或开放浏览器安装代码。禁止冲突覆盖，原生命令和已有别名优先。动态目录来自同一个注册表，QQ 和 HTTP 不另维护插件白名单或处理器副本。

handler 收到 CommandContext：会话 ID、冻结远端 cwd、历史、已校验参数、取消令牌、受计量的模型，以及 `dep::<peri_cloud::commands::CloudCommandScope>()` 中的已认证 principal/session/turn/device。没有 SSH 凭据、运输令牌或聚合管理 PAT。cwd 是远端坐标，不能据此在云主控本地访问文件。原生 ArgsSchema 的 Path 在本机默认检查存在性；云端共用解析器只验证参数形态，将 Path 视为远端路径值，存在性必须由 handler 通过认证远端工具检查，不能在主控文件系统判断。

- Done：反馈并原子提交经过校验的上下文变化；旧消息不改写或重排，移除项标排除。UiOnly 不进入模型上下文；Session 反馈追加到尾部 Human 消息，不原地改写冻结 System Prompt。
- Inject：沿用原 Agent loop、原 PermissionMiddleware、远端工具与计量。
- Delegate：没有已装配目标时明确失败，不伪造成功或把委派文本交给 LLM。

自定义 handler 属于可信服务端代码；需要观察 cancel_token。外部工具副作用仍必须走原工具审批与认证执行链，不能用命令注册绕过权限。宿主不尝试通过取消 future 证明外部副作用已回滚。

## 验证与升级

Windows 行为回归覆盖同目录新会话、重复事件/请求、权限与历史保留、重启后恢复、真实摘要与后续模型输入、失败压缩、运行中拒绝、clear/rewind、动态注册/冲突/Inject、取消、账号隔离、HTTP Cookie/CSRF/确认。无新增数据库 schema 或常驻进程，执行器协议不变。

升级只替换主控二进制并保留状态；回滚使用同 schema 的 v0.6.0 主控，但回滚后不可继续使用新增 slash 命令。历史 flags 是既有格式，旧版仍能读取已压缩上下文；原始历史和轮次记录保留。不要覆盖升级后的数据库来回退代码。
