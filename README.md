# Peri Cloud

个人云 Agent：推理循环在服务器，工具在你的 Windows / Linux 设备执行，QQ 是可选的聊天入口。

本项目基于 [Peri](https://github.com/KonghaYao/peri) 的 Rust Agent 核心，拆出原生执行器并加入账号、设备连接、网关和常驻服务。发布包不包含 TUI。保留依赖所需的编译期提示词与 workflow 资源。首次公开版本为 `v0.2.0`，仍处于预览阶段。

## 能做什么

- 云端运行原有 Agent 推理循环；模型使用兼容 OpenAI Chat Completions 的接口。
- Windows 和 Linux 执行器提供 Read、Write、Edit、Glob、Grep、Bash，复用同一套工具代码。
- 在个人电脑浏览器登录，使用 OAuth 授权码 + PKCE S256 绑定执行器；Windows 登录凭据使用 DPAPI，Linux 使用权限受限的文件。
- 在网页生成五分钟有效的一次性口令，在 QQ 发送 `/绑定 口令`，再回到网页核对并确认聊天账号。
- QQ 显示 AI 回复；需要审批时发送简短工具摘要及“允许一次 / 拒绝”按钮，完整参数可在账号网页查看。
- 权限属于当前聊天会话，云端复用 Peri 的工具权限机制。执行器不重复弹出工具审批。
- 原生 HTTP 工具协议运行于回环地址，通过 SSH 正向或反向隧道连接；SSH 地址、密钥和运输令牌不进入模型参数。
- 持久化会话、任务、审批及发件记录；连接中断或结果未知时，不盲目重新执行工具或重复发消息。
- 账号工作台提供设备撤销、聊天账号移除、关联请求确认、操作审批和云服务关闭入口。

## 下载与构建

在 [v0.2.0 Release](https://github.com/Teens-in-Times/peri-cloud/releases/tag/v0.2.0) 下载 Windows x86_64 或 Linux x86_64 压缩包，并使用 `SHA256SUMS` 校验。Linux 二进制在 Ubuntu 22.04 x86_64 上构建、验收；其他架构请从源码编译。

维护者发布 Windows 二进制时可使用 `scripts/build-release.ps1 -CargoHome C:\BuildCache\peri-cloud`。使用不含个人账号名的 Cargo 缓存目录，避免 C 依赖记录个人源码路径；MSVC 的 PDB 引用由构建脚本保留为文件名，参见 [Microsoft 文档](https://learn.microsoft.com/en-us/cpp/build/reference/pdbaltpath-use-alternate-pdb-path)。`PDB` 文件不进入发布包。

发布包包含 `peri-cloud-host`、`peri-executor`、许可证、配置示例和文档。配置示例中的域名、模型名称和设备 UUID 都是占位值，下载后需要自行配置。

需要 Rust 1.99+、平台 C/C++ 编译工具和 OpenSSH 客户端。Windows 构建使用 MSVC；Linux 构建需要 C/C++ 编译器、CMake、pkg-config 等基础开发工具。

```sh
cargo build --release --locked -p peri-cloud --bin peri-cloud-host -p peri-executor --bin peri-executor
```

## 开始使用

1. 在云服务器配置模型、QQ 环境变量以及 `host.json`，启动 `peri-cloud-host --config host.json`。
2. 在个人电脑启动执行器，并配置设备发起的反向 SSH 隧道。
3. 使用执行器的 `login` 入口，在本机浏览器创建 / 登录账号并确认设备授权。
4. 把执行器设备 UUID 与运输令牌配置到云端连接表。OAuth 登记设备和实际 SSH 通路是两个步骤；登记成功不代表执行器已经在线。
5. 在工作台生成关联口令，在 QQ 绑定后回到网页确认；发送 `/电脑` 和 `/连接 电脑ID`，然后直接交代任务。

完整命令与令牌配置见 [部署指南](docs/setup.md)，架构与权限边界见 [架构说明](docs/architecture.md)。

## QQ 常用操作

| 命令 | 用途 |
| --- | --- |
| `/帮助` | 查看命令 |
| `/绑定 口令` | 请求关联当前聊天账号 |
| `/电脑` | 列出已授权设备 |
| `/连接 电脑ID` | 选择当前会话的执行设备 |
| `/权限 默认` | 默认权限模式，按工具策略审批 |
| `/权限 编辑` | Peri 的编辑权限模式 |
| `/权限 全部` | 当前会话允许全部工具；仅用于你信任的任务 |
| `/状态` | 查看会话状态 |
| `/取消` | 请求停止当前任务 |

QQ 原生按钮是否可用取决于机器人平台权限。网关按平台实际提供的点击者身份验证审批归属；按钮使用 Hermes 兼容的权限类型，后台仍严格核对账号、会话、设备、请求和期限。平台明确拒绝原生卡片时使用网页审批入口；投递结果未知时不重复发送。

## 验证

核心验证命令：

```sh
cargo test --locked -p peri-cloud -p peri-executor -p peri-remote-tools -p peri-tool-runtime -p peri-process
cargo clippy --locked -p peri-cloud -p peri-executor --all-targets --no-deps -- -D warnings
```

测试使用模拟模型与 QQ 服务，不需要真实 API 密钥。JS 执行及其他继承的扩展模块可能需要各自的额外运行环境；上述测试聚焦本项目交付的云端和设备执行链路。

## 当前限制

- 尚未提供远程开机 / Wake-on-LAN 的常驻家中节点。
- Linux 核心工具可运行，完全无人值守服务器的 OAuth 登录体验仍需完善。
- Windows 提供用户登录后的启动方式，尚未提供经过验收的登录前系统服务安装器。
- 设备连接和运输令牌目前由部署者配置，尚未提供自动发放连接配置的向导。
- QQ 是目前实现的消息适配器，微信适配器和长期记忆后端属于后续扩展。
- 原生 QQ 按钮已有协议、归属验证和进程集成测试；不同机器人账号的真实平台权限需要分别验收。

执行器以启动它的系统账号运行，能够访问该账号可访问的文件并执行命令；它不是安全沙箱。会话审批是工具权限入口。只绑定你控制的聊天账号和设备，并保护执行器运输令牌、SSH 密钥及云端配置。

## 来源与许可

Apache-2.0。源代码基于 Peri 提交 `d7ee444efe7a461b0696bd6c27c6c91f946fdec1`，新增云端、执行器、远程工具协议和共享工具运行库，并修改必要的依赖模块。QQ 官方网关实现参考独立项目 Prism 的适配协议；按钮行为参考 [Hermes Agent](https://github.com/NousResearch/hermes-agent) 的网关实现。

参见 [LICENSE](LICENSE) 与 [NOTICE](NOTICE)。第三方依赖保留各自许可证。公开仓库采用新的源码快照，未包含原开发目录的 Git 历史、个人配置、聊天记录、凭据或运行数据库。