# 部署指南

## 云服务器

复制发布包中的 `peri-cloud-host`、`examples/host.json` 和 `scripts/peri-cloud.service` 到自己的部署目录。示例 systemd 单元使用如下布局：

- `/opt/peri-cloud/current/peri-cloud-host`：可执行文件。
- `/etc/peri-cloud/host.json`：配置。
- `/etc/peri-cloud/service.env`：仅部署者与服务账号可读的环境变量文件。
- `/var/lib/peri-cloud`：仅服务账号可写的运行状态。

创建专用 `peri-cloud` 系统账号，赋予状态目录权限，使用示例服务单元或等价的进程管理器。环境变量包括：

```text
PERI_BOOTSTRAP_TOKEN=<用密码学安全随机数生成、至少 32 字符的初始化口令>
PERI_MODEL_API_KEY=<自己的模型 API 密钥>
PERI_QQ_APP_ID=<自己的 QQ 官方机器人 AppID>
PERI_QQ_CLIENT_SECRET=<自己的 QQ 官方机器人密钥>
```

初始化口令用于首次创建个人账号，不能放入公开配置或聊天消息。宿主启动仍需要该环境变量；账号创建后不能重复初始化。密钥只通过配置中的环境变量名引用。

把模型 API 地址和模型名称换成自己的供应商配置。仅在供应商明确支持时打开 `supports_thinking_content`。不使用 QQ 时把 `qq` 设为 `null`，电脑账号和 OAuth 功能仍然保留。

```sh
peri-cloud-host --config /etc/peri-cloud/host.json --check-config
peri-cloud-host --config /etc/peri-cloud/host.json
```

`--check-config` 只校验结构，不读取密钥，也不证明模型、QQ 或执行器在线。

网页可以通过设备侧 SSH 正向转发访问 `http://127.0.0.1:47671`。若部署 HTTPS 反向代理，需要把 `public_origin` 设为实际访问地址，并正确传递 Host；Cookie、Origin 和 CSRF 校验依赖它。HTTP 仅允许回环地址。

## 设备与 SSH

设备上的 OpenSSH 配置使用自己创建的别名，例如 `agent-cloud`。先人工验证服务器主机密钥，使 `ssh -o BatchMode=yes agent-cloud` 可以用密钥登录；执行器不绕过 known_hosts 验证。

为隧道账号启用所需的正向 / 反向 TCP 转发，并让反向监听保持回环地址。每台设备需要独立的云端反向端口；下例只有一台设备。

Windows PowerShell：

```powershell
$deviceState = Join-Path $env:LOCALAPPDATA 'PeriCloud\state'
.\peri-executor.exe --state-dir $deviceState --device-name 'Workstation' --listen 127.0.0.1:42371 --ssh-host agent-cloud --ssh-reverse-listen 127.0.0.1:44371 --cloud-listen 127.0.0.1:47671 --cloud-remote 127.0.0.1:47671
```

Linux：

```sh
peri-executor --state-dir ./device-state --device-name Workstation --listen 127.0.0.1:42371 --ssh-host agent-cloud --ssh-reverse-listen 127.0.0.1:44371 --cloud-listen 127.0.0.1:47671 --cloud-remote 127.0.0.1:47671
```

执行器不需要模型 API 密钥或 QQ 密钥。设备状态目录包含运输令牌和任务数据库，请放在仅该系统账号可访问的位置。

## 电脑登录与绑定

执行器保持运行，在另一个终端登录；`--state-dir` 必须与常驻执行器一致，工作区应为实际存在的绝对路径。

Windows：

```powershell
.\peri-executor.exe --state-dir $deviceState --device-name 'Workstation' login --cloud-url http://127.0.0.1:47671 --workspace C:\Workspaces\project
```

Linux 桌面：

```sh
peri-executor --state-dir ./device-state --device-name Workstation login --cloud-url http://127.0.0.1:47671 --workspace /srv/workspace
```

在本机浏览器创建 / 登录账号并确认设备连接。用户名只接受英文字母、数字、下划线、连字符和点；密码至少 12 字符。首次创建账号还需输入部署者生成的初始化口令。

请保持登录终端等待回调。授权码和 state 参数是临时凭据，不要复制到聊天中。`--no-browser` 只禁止自动打开浏览器，仍需在发起登录的设备上完成回环回调，不是无人值守登录方案。

其他账号命令：

```sh
peri-executor --state-dir ./device-state login-status --cloud-url http://127.0.0.1:47671 --workspace /srv/workspace
peri-executor --state-dir ./device-state register --cloud-url http://127.0.0.1:47671 --workspace /srv/workspace
peri-executor --state-dir ./device-state forget-login --cloud-url http://127.0.0.1:47671 --workspace /srv/workspace
```

`forget-login` 只删除本机登录凭据；撤销远端设备授权请在账号工作台操作。

## 配置实际执行通路

登记设备后，读取设备状态目录 `device.json` 中的 UUID。由部署者通过可信的管理连接，将该目录中的 `transport-token` 文件安全复制到云端，例如 `/etc/peri-cloud/devices/workstation.token`，权限仅允许管理者及服务账号读取。不要把令牌粘贴到模型、QQ 或公开 Git 仓库。

在云端 `host.json` 的 `devices` 中加入：

```json
{
  "device_id": "00000000-0000-0000-0000-000000000001",
  "endpoint": "127.0.0.1:44371",
  "token_file": "/etc/peri-cloud/devices/workstation.token"
}
```

UUID 是占位值，必须替换为实际设备 UUID。这里的端口对应设备启动参数 `--ssh-reverse-listen`。重启云宿主以读取新的连接配置。若云端发起正向 SSH，则使用 `devices[].ssh` 配置现有 SSH 别名及远端回环执行器地址，详见源码配置类型。

## QQ 关联与任务

1. 在工作台生成一次性关联口令。
2. 发给官方机器人：`/绑定 口令`。
3. 回到工作台核对完整聊天身份，点击确认关联。
4. 发送 `/电脑`，再发送 `/连接 电脑ID`。
5. 保持默认权限，交代一个简单任务并在 QQ 或工作台审批。

关联只确认账号所有权，设备仍需要真实在线连接。QQ 平台可能要求机器人账号具有对应消息、Markdown 与交互权限。WebSocket 模式主动连接官方 QQ，不需要公开回调端口；Webhook 模式另需正确配置平台回调。

## 指定与切换工作区

工作区属于具体聊天会话。首次连接可发送 `/连接 电脑ID C:\Projects\demo` 或 `/连接 电脑ID /srv/demo`；省略路径时使用设备登记的默认工作区。路径必须是设备上已经存在、可以访问的绝对目录，可以包含空格，不需额外引号。目录由执行器按自己的操作系统验证和规范化。

发送 `/工作区` 查看当前及已有路径，`/工作区 绝对路径` 切换。网页“会话工作区”展示每个聊天会话的设备和当前路径；点击“切换工作区”，可以选择已有目录或填写新路径。这里只改变选中聊天会话的工作区，不改设备的登录默认目录。

切换会开始一个新的 Agent 会话，继承原权限模式；原会话历史和冻结路径仍留存，不自动加载到新会话。其他聊天会话各自保留工作区。任务运行中、审批等待中或执行结果未知时禁止切换。无效目录、断线或过期页面请求不会改变原选择；刷新后以显示的实际路径为准。

## 常驻

云端可安装示例 `peri-cloud.service` 并使用 `systemctl enable --now peri-cloud`。`/healthz` 只表示 HTTP 进程正在提供服务；模型、QQ 心跳及执行器认证需要分别检查。

Windows 的通用启动包装器为 `scripts/run-cloud-executor.ps1 -InstallRoot <安装目录>`，目录需包含 `current/peri-executor.exe`、`executor.json`、`state/` 和 `logs/`。`examples/executor.json` 展示所需字段。包装器只重启退出的执行器，不重新提交工具任务；可以放入当前用户的登录启动项。它不是登录前系统服务安装器。

Linux 执行器可以用独立 systemd 用户服务或系统服务常驻，使用最低必要的系统账号权限、明确状态目录和网络依赖。服务终止应给执行器结算已有任务的时间；不要把强制杀进程等同于安全取消。

Windows 持久凭据使用 DPAPI，Linux 登录文件限制为当前账号读写。自行备份 SQLite 状态时请同时考虑日志、令牌和账号隐私；这些文件不属于 Release。