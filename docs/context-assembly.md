# 上下文装配、工作区与环境快照

本次只扩展上下文资料的装配，不改变原 Agent loop、PermissionMiddleware、OAuth 或设备调用身份。未运行全测；定向验证结果在文末补充。

## 正文、动态资料和工具

冻结正文默认顺序为 base → role → soul → channel → memory_protocol → files → extensions → tools。工具 schema 独立注册；tools 不是把完整工具目录重复写进正文。

ContextAssembler 记录每段 key、来源类型、角色、字节数、工具数和内容/schema 版本。config.context.section_budgets 可限制同 key 的累计正文大小；max_bytes 仍是总正文上限。来源清单只写诊断字段，不记录正文或凭据。

base/role 支持受控一层宏替换：{{assistant.role}} 和 {{channel.constraints}}。未知或未闭合宏失败；替换值不再求值。SOUL、项目资料、技能和工具结果都不作为模板执行，保留实际文本。任务和环境适合用动态帧，不应塞入稳定前缀。

动态后缀继续经过原缓存边界。记忆、任务、MCP 状态由原来源帧维护；没有将完整 trace、技能正文或历史复制进 system。

## SOUL 与项目指令

SOUL 是助手级：

- 部署者：context.directory/SOUL.md。
- 账号私有偏好：context.directory/users/<principal>/SOUL.md，仅相同可信账号的私人路由。
- 未指定 context.directory 时使用 host 状态目录下的 context。
- 不读取执行器 cwd/SOUL.md，不从项目改变助手身份。

项目指令由已认证执行器按绑定 cwd 提供，主文件依次选第一个可读取的非空候选 AGENTS.md、CLAUDE.md、.claude/AGENTS.md；优先 AGENTS.md，不叠加后面的主候选。CLAUDE.local.md 是独立补充。没有向 cwd 父目录、其他工作区或云宿主文件系统回退；没有自动展开 @import。

内容携带原路径、sha256 和裁剪标记。项目规则和目录在执行器绑定内保持快照，切换工作区或建立新执行窗口重新获取。按调用准备其他设备时，资料只附着相应不可变 target，不能改写原目标。

## 全局与项目 Skill

| 范围 | 安装位置 | 可见性 |
| --- | --- | --- |
| 项目 | 执行器 cwd/.agents/skills 或 .claude/skills | 当前认证设备/工作区 |
| 账号全局 | context.directory/users/<principal>/skills/<name>/SKILL.md | 本账号私人路由 |
| 管理员全局 | context.directory/skills/<name>/SKILL.md | 明确安装的受控公共资料 |

同相对目录名优先 .agents/skills；同技能名优先项目 → 账号 → 管理员。被遮蔽的全局条目保留 account:/operator: 限定名及虚拟目录路径，避免默默读错来源。全局根不自动指向云宿主 ~/.claude/skills 或 ~/.agents/skills。

仅向模型提供有界目录；SkillTool 按名称/目录路径读取 SKILL.md，DiscoverSkillsTool 可过滤。项目目录随绑定固定；全局目录每回合重建，正文按调用读取。全局读取拒绝跨账号、路径逃逸和符号链接，正文上限 32 KiB；全局目录最多 128 条，扫描深度、目录数和名称大小有界。

全局 Skill 是云端指南，虚拟路径不是本地执行器文件路径。此版提供 SKILL.md 正文，辅助文件不会自动导入或执行；需要运行脚本时须将所需资料明确放到目标工作区，并使用原权限链。执行者可以使用已装配的 Skill 读取能力，不能借技能扩大权限或递归委派。

## MCP

保持现有账号独立的 MCP pool、连接版本刷新、服务状态消息和延迟工具/资源发现。MCP 工具和资源内容不获得用户指令或审批权威。默认未启用的服务不会因为项目文字提到它就连接。

本次补了明确的能力加载协议，未增加自动读取 .mcp.json、云宿主全局 MCP、stdio 插件或把端点凭据放进模型参数的路径。项目级 MCP 的自动准入涉及授权范围，留待明确的账号配置流程。

## 环境快照

新增 feature environment-read@1 和认证接口 GET /v1/sessions/{id}/environment。每次读取核验原 session/工作区文件系统身份，不创建工具 job。

字段包含 device_id、原 binding、platform、architecture、实际 shell、observed_at，以及固定应用清单的 executable/version。PATH 只检查最多 128 个绝对目录的固定文件名，不执行二进制、Shell、配置文件或脚本；version 未探测为 null。Linux 检查执行权限，Windows 检查 exe/cmd/bat 候选。

当前固定清单：git、cargo、rustc、node、python、python3、bun、go、docker。需要应用版本或清单外能力时通过普通受授权工具检查，不能将文件存在当作安装成功或可用性证明。

云端验证设备、原 binding、平台、数量与大小；RemoteSession 按该绑定缓存五分钟。普通回合和 prepare_device 都有两秒读取预算，能力缺失/超时只缺席这份资料。快照标明观察时间，不作为在线证明，不自动写个人记忆。目标选择不会改变旧调用的环境和操作归属。

## 创作与清理

编辑目录为 docs/prompt-review。每份 Markdown 顶部都说明功能、实际作用位置、建议内容及宏边界。说明放在 PERI-EDIT-GUIDE:BEGIN/END 标记内，运行中的 peri-cloud/prompts 文件不包含这些编辑说明。

先改 PROMPTS-EDIT.md 或对应分文件，优先选一种。助手 SOUL 和项目指令的创作位置分别为 16-assistant-soul.md 与 17-project-instructions.md。运行提示词中的默认基座、角色、审批、记忆、扩展协议已集中在 peri-cloud/prompts。

scripts/prepare-prompt-review.py 更新未改动的导出稿，会保留用户修改。--clean-to <新目录> 只复制并去掉编辑说明，不覆盖创作稿、不部署、不把参考文件自动装配为运行规则。

## 参考来源

历史压缩与个人记忆是两条不同路径。主会话可用 Micro 投影收起旧工具交换，通过 ReadContextArchive 按引用、预算和游标回查；Full 摘要保留有效归档根。归档读取绑定当前会话，重新验证用户、上下文代次和隐私边界，隐藏推理不开放为工具结果。执行者继续以持久任务事实和原窗口交接恢复。接口及验收见 [归档同步记录](upstream/peri-e598d178.md)。

本次吸收目录分层与按需加载的方式；Peri 的具体优先级、私聊边界和助手级 SOUL 按本项目约定实现。

- [Claude Code Skills](https://code.claude.com/docs/en/skills)：个人/项目技能及按需正文。
- [Claude Code memory](https://code.claude.com/docs/en/memory)：项目指令的层级和作用范围。
- [Claude Code MCP](https://code.claude.com/docs/en/mcp)：连接配置范围与发现。
- [OpenClaw agent runtime](https://github.com/openclaw/openclaw/blob/main/docs/concepts/agent.md)：助手 bootstrap 资料和有来源的 Skill 目录。
- 本机 ClaudeCodeBest 的 prompts.ts、systemPromptSections.ts 与 SessionMemory/prompts.ts：分段装配、刷新频率及局部宏替换。只读调查，未修改该仓库。

## 定向验证

已完成 48 项不同的定向回归，0 失败；未运行全测。Cloud 的 context/workspace/assembly/MCP 37 项，模拟模型真实执行器的上下文注入、跨设备目标和低预算换窗 3 项，Executor 工作区/环境/认证 HTTP 8 项。最后的缓存边界硬化后，工作区 14 项、上下文注入及低预算换窗再次通过。

所有 Rust 检查均使用 scripts/cargo-rmcp-patched.sh。cargo fmt --all -- --check 和 peri-cloud、peri-executor、peri-remote-tools、peri-acp-types 全目标严格 Clippy 通过。上游检查保持 1291 项、1264 项原样、27 项允许补丁；依赖清单、Cargo.lock、许可证和 NOTICE 未改动。

提示词编辑流程另外验证：所有 84 份 Markdown 含编辑说明；重复刷新两次保留分文件与合订稿创作；清理副本移除说明并保留创作；已有输出目录拒绝覆盖。来源哈希、导出哈希、索引链接、ZIP 完整性和变更源码小于 1000 行均已检查。没有生产部署。
