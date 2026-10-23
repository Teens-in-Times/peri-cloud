# 验证记录

## 上游更新到 e598d178（本地开发验证，2026-10-24）

本轮覆盖上下文归档、异步结果消费保护与依赖更新，保留云端任务、操作账本和工作区授权。修复后按测试目标去重为 5955 通过、0 失败、42 忽略；包含共享核心库和 README 六 crate，未运行完整上游 TUI / ACP 产品。格式、工作区库及六 crate 全目标严格 Clippy、来源清单与文件行数约束通过。精确范围、初始失败修复和未验证项见[本轮同步记录](upstream/peri-e598d178.md)。

Cloud schema 25；未真实调用模型、未生产升级、未发布新制品，不继承 rc.1 的生产验收结论。

## 上游更新到 7d5295c7（本地开发验证，2026-10-19）

- 上游从 `e1d1b9c` 更新到 `7d5295c7`：5 个提交、395 个文件；叠加范围内 154 个文件（127 修改、27 新增、0 删除）。
- 新 manifest 1291 个文件：1264 个与上游一致，27 个允许清单内的生产补丁，0 缺失、0 未声明漂移。处理见 [upstream/peri-7d5295c7.md](upstream/peri-7d5295c7.md)。
- `transcript.rs` 的 2 处冲突按 deployment-owned journal 语义保留 `canonical_payloads()` 与 shrink 守卫，同时接收上游内存观测。
- `EventSink` 返回类型、`GoalViewSnapshot.goal_id`、`complete_goal_if_current`（goal_id CAS）已适配，并补充 CAS 回归测试。
- `cargo check --workspace --all-targets` 通过。
- 云端库与集成测试 228 项通过；`peri-acp-types` 665 项、`peri-agent` 1169 项（4 ignored）、`peri-middlewares` 1785 项（2 ignored）通过。
- 执行器库 16 项通过；HTTP 集成 7 项通过，仅既有 Windows PowerShell 编码测试失败；`peri-remote-tools` 6 项通过。
- `cargo fmt --all --check` 与 scheduler/cloud/executor 严格 Clippy 通过。
- 未构建 release，未部署生产；真实执行器 compact/重启与 MCP 网络行为仍需部署前冒烟。

## 分范围记忆与上下文帧（本地开发验证，2026-10-18）

- README 六 crate 回归：35 个测试目标，428 通过，0 失败、0 忽略。
- 模型出口补充变更后，云端库 71 项和原 Agent loop 9 项复测通过；撤销设备资料管理补充后，记忆专项 20 项通过。以上复测与完整回归重叠，不累加计数。
- 覆盖 USER/FLEET/DEVICE_ENV 隔离、设备撤销及本人清理、原权限审批、帧复用与补发、预算观测与实际请求一致、遗忘后的工具结果/摘要/AI 转述隔离、回退及数据库重开恢复、原生命令历史隔离、QQ 确认与 HTTP 同源/CSRF。
- 三 crate 全目标严格 Clippy、cargo fmt、四套网页 DOM 检查通过；撤销范围只读及遗忘入口另经记忆网页测试复测。
- 上游清单检查：1264 个文件，1241 个一致，23 个允许的生产补丁；源文件及测试均低于 1000 行。
- 这是本地源码验证，没有部署或真实个人 QQ 投送验收。CONTEXT、后台自省、候选箱与 Skill 晋升尚未交付，运行边界见 [记忆系统](memory.md)。

## v0.7.1 部署（2026-10-18）

- 发布源码：`ebde31d2bd7de794750bc9e187e1cdec5a5c4619`；[完整 CI](https://github.com/Teens-in-Times/peri-cloud/actions/runs/39098821796)；Release 资产含 Linux/Windows 包、源码 zip 与 `SHA256SUMS`。
- 修复：原生与令牌端点除公网 authority 外，也接受宿主自身回环监听地址；浏览器路由仍只接受公网 authority。新增单测覆盖回环地址、非原生路由、伪造 Host 和未配置监听地址四种情况；`portal` 集成测试 5 项通过。
- 上游来源校验：1264 个文件，1241 一致，23 个允许清单内的生产补丁，0 缺失、0 未声明漂移。
- 东京生产主控切换到 `releases/v0.7.1-ebde31d`；部署前备份 `/var/backups/peri-cloud/v071-20261018-133202`（SQLite backup API 数据库 + `/etc/peri-cloud*` 打包）。
- 切换后 schema 保持 9：`user_version=9`、`application_id=1346720323`、31 张表；`/healthz` 与公网门户返回 200；回环浏览器路由返回 403，回环原生路由在无凭据时返回 401。
- Windows 执行器恢复上报：`device_connections` 出现 `feac1160-16b6-4fa3-a4d5-c89de5785190`，endpoint `127.0.0.1:44371`，租约按 30 秒续期；宿主到该回环端点的 TCP 连接成功。
- 首次上报的阻塞来自旧版 guard 403 时执行器已把本地凭据标记为 `refresh_pending`；该标志按设计会阻止后续刷新。清标记前已备份本地 DPAPI vault，仅重置该布尔值，未读取或改写任何令牌；刷新随后成功。
- 执行器与东京/北京静态连接未替换：v0.7.0 执行器已含上报代码，本次只升级宿主。
- 2026-10-18 门户迁移：新增 `peri.claudeoutofdario.icu` A 记录与 Let's Encrypt 证书，nginx 反向代理到回环 47671，`public_origin` 切换为新域名；新域名门户与 `/healthz` 返回 200，旧 `www` 返回 403；切换后设备租约继续续期，QQ/OneBot 重新认证。
- Windows 执行器的 `executor.json` 登录地址同步改为新域名；回环上报路径不受影响。

## v0.7.0 升级准备（2026-10-18）

- 基线：`0941d0683802f25e21ff8b0aa8c894050e3b9377`（v0.6.1）。
- v4 核心叠加到 `e1d1b9c034092e4e58c163f1a8e5005a1db5f68c`：上游 manifest 1264 个文件，1244 个一致，20 个允许清单内的生产补丁，0 缺失、0 未声明漂移。
- 升级后 Windows 串行回归：30 个测试二进制，371 通过，0 失败；`cargo check --workspace --all-targets` 通过。
- Peri v4 核心回归：32 个测试二进制，4509 通过，0 失败；`peri-cloud` 与 `peri-executor` 的严格 Clippy 通过。
- 设备自动接入、账户/设备生命周期与 Web 对话持久化回归：30 个测试二进制，381 通过，0 失败；schema 9 迁移与本地 schema 6 拒绝测试通过。
- 手动 `/compact` 只生成摘要并由云 journal 结算；自动 Full Compact 使用 deployment-owned 模式，热 transcript 不提前释放被排除历史，避免 journal 丢失 canonical 消息。
- Windows 串行基线（升级前）：29 个测试二进制，371 通过，0 失败；格式检查通过；portal/model DOM 检查通过。
- Windows 控制台须为 UTF-8（`chcp 65001`）。否则 executor 的 PowerShell 中文输出会按本地代码页解码，`shell_alias_captures_stdout_stderr_and_nonzero_exit_evidence` 会失败；这是测试机代码页问题，不是代码回归。CI 的英文 runner 不受影响。
- schema 8 fixture 由生产库 DDL 生成（不包含数据），28 张表，`PRAGMA integrity_check` 通过。
- v4 上游清单与生产补丁清单见 [upstream/peri-e1d1b9c0.md](upstream/peri-e1d1b9c0.md)。

## v0.5.0

- 发布源码：`638bba6f5a7fcbdfd89e266d96add3e40fc02900`；[完整 CI](https://github.com/Teens-in-Times/peri-cloud/actions/runs/38961279120)。
- Windows 云端、执行器、远程工具、共享工具与进程回归：352 项通过，0 失败，0 忽略；格式与严格 Clippy 通过。
- Linux 原生发布构建、实际进程启动、账号鉴权、嵌入资源和隔离执行器验收通过；按当前维护约定不重复 Windows 已通过的整套测试。
- 新增访问控制行为包括管理员私聊与网页共用策略、用户和群的联合白名单、黑名单优先、过期与旧版本拒绝、重启持久化、禁止跨账号管理，以及名单收紧后抑制待发送结果。
- 网页运行实际 HTML/JS 的 DOM 行为验收，覆盖标识转义、明确确认后写入、版本核对、取消和登出清理。
- schema 6 升级至 7 的生产部署保留原有 21 张表；二次发布替换保留升级后全部 24 张表。部署前保存数据库和配置备份，回滚同时恢复对应 schema 的数据库。
- 正式 Release 的三个安装 / 源码包下载后重新校验 SHA256SUMS、BUILD.json 和精确 Git 提交；运行宿主及三处执行器的二进制与正式发布包一致。
- 官方 QQ 与个人 OneBot 在实际部署重新建立已认证连接。三处执行器均通过真实 `/v1/info` 身份与版本认证；远端两条 SSH 备用通路也通过认证，WireGuard 通路保留。
- 个人 QQ 的协议测试和实际登录不等同于客户端菜单、审批及长期稳定性全部验收；首次聊天身份仍须关联 Peri 账号。默认会话权限仍为审批，部署者的显式高权限选择不改变产品的全局默认值。

v0.5.0 没有包含检索式记忆和自创 Skill；当前个人条目记忆与中文检索已另行实现，见 [记忆初版](memory.md)。自创 Skill 仍属后续设计。

## v0.4.0

- Windows 云端、执行器、远程工具、共享工具与进程回归：312 项通过。
- Ubuntu 22.04 同一组回归：327 项通过。
- 本地模型协议及传输模块：163 项通过。
- 两平台云端 / 执行器 Clippy 和格式检查通过。
- Linux 发布二进制的真实 HTTP 登录、嵌入资源、运输认证与关闭验收通过。
- 独立容器的真实 Shell、专用目录写入、非 root 身份、只读根目录、挂载隔离、回环认证和资源限制通过。
- 北京服务器的 v3 数据副本升级至 v6，原账号、设备、OAuth 授权、QQ 关联及历史会话保留。
- 网页实际检查管理员 / 成员切换、个人记忆、个人模型保存及密钥不回显；成员不显示管理员实例和管理入口。
- 公开源码 Gitleaks 无发现；Linux 发布二进制额外检查实际部署私密值，无匹配。

资源额度统计逻辑模型请求，不是准确 token 账单。真实 QQ 连接在升级后恢复；平台消息、按钮权限仍需按各机器人账号确认。

Linux Docker internal 网络未发布回环端口，现使用禁用容器互访的专用 bridge；包含实际容器验收。Windows Shell 的退出状态测试设置明确的前台等待时限，避免共享 CI 的 PowerShell 冷启动超过默认 15 秒。产品的默认超时行为未改变。

## v0.2.0

首次公开快照基于 Peri `d7ee444efe7a461b0696bd6c27c6c91f946fdec1`，使用 Rust 1.99.0 构建。

| 项目 | 结果 |
| --- | --- |
| Windows x86_64 云端、执行器、远程工具、共享工具与进程测试 | 290 通过，0 失败，0 忽略 |
| Ubuntu 22.04 x86_64 同一组测试 | 305 通过，0 失败，0 忽略 |
| Windows / Linux Clippy，云端与执行器全部目标 | 通过 |
| Rust 格式检查 | 通过 |
| Windows / Linux 优化发布构建 | 通过 |
| Windows / Linux 实际发布二进制启动与 HTTP 登录 | 通过 |
| 实际发布二进制的嵌入 CSS / JS 与源码一致 | 通过 |
| 发布二进制的账号鉴权关闭与执行器运输鉴权 | 通过 |
| 桌面和 390px 窄屏账号界面检查 | 通过 |
| 模拟网页审批、折叠参数和一次性口令入口 | 通过 |
| 网页 / QQ 指定与切换工作区，含空格目录和真实远程写入 | 通过 |
| 工作区权限继承、旧会话历史冻结、重启恢复 | 通过 |
| 工作区登录 / CSRF / 过期选择、运行中及撤销后拒绝切换 | 通过 |
| 源码 Gitleaks 扫描 | 无发现 |
| 源码与二进制针对原部署私密值的额外扫描 | 无匹配 |

Linux 发布构建移到 Ubuntu 22.04 GitHub CI，避免占用运行服务的服务器资源；下载的 Linux 二进制通过独立的 Ubuntu 22.04 验收任务验证实际启动与账号接口。Linux 文件权限测试使用普通账号、可写的独立测试工作目录，避免 root 权限绕过只读文件的预期错误。测试实际流程还包括 OAuth PKCE、设备撤销、QQ 签名与消息去重、审批归属、重复点击、执行结果未知、任务取消与重启恢复。

隐私清理使用新的单次源码快照，排除原 Git 历史、个人部署说明、配置、运行日志、数据库、账号状态、SSH 密钥及运输令牌。发布二进制使用源码路径映射和中性的 Cargo 缓存，Windows PDB 引用保留文件名，PDB 文件不上传。

本机完整测试曾因系统临时目录所在磁盘空间不足而失败；改为本次任务的独立可写临时目录后完整通过，未修改或跳过该测试。

构建验证使用模拟模型与 QQ；不会把协议模拟测试当作所有真实 QQ 机器人账号的原生按钮权限验收。预览版的当前功能限制见 README。
