# 多用户部署

一个云宿主服务多个独立用户，每人拥有自己的账号、聊天身份、设备池、对话、审批、记忆和模型配置。各会话使用相同 Agent 核心代码创建独立推理循环，不同用户可并行。

## 邀请与设备

原账号升级后仍是管理员。在工作台“邀请新成员”生成注册链接，成员在 24 小时内设置用户名和密码。链接只可注册一次，也可撤销。成员在自己的执行器完成 OAuth 授权，由部署者配置真实 SSH / 运输通路，再在自己的工作台生成 QQ 关联口令并确认关联。

同一用户可以绑定多个 QQ / 网关身份，共用自己的设备池和记忆；各聊天会话独立选择实例、工作区及权限。服务器本地实例仅原管理员可见和使用，成员不能通过猜测 UUID 浏览目录，也不能邀请用户或关闭服务。

## 个人模型与上下文

服务器 `model` 保留为公共默认。用户在“我的模型”填写供应商、模型 ID 和自有 API Key，下一轮生效。密钥留空仅在供应商不变且已有密钥时表示保留，切换供应商需要新密钥。“使用服务默认”删除本人覆盖配置。

部署者可在 `host.json` 增加供应商：

```json
"model_providers": [
  {"id":"alternative","name":"另一个模型服务","api_base":"https://api.example.net/v1"}
]
```

原 `model.api_base` 自动以 `default` 供应商出现，不能重复声明。地址要求 HTTPS，字面回环地址允许 HTTP，用户不能填写任意 API 地址。当前使用 OpenAI Chat Completions 兼容协议，上下文窗口及输出上限仍由宿主统一配置。

密钥不回显，不进入聊天、工具参数或诊断输出。当前保存于受保护的 SQLite 状态目录中，数据库及备份应按凭据保护；尚未提供专用密钥库或字段加密。

网页支持个人回复偏好和长期记忆；聊天也可让 Agent 用 `ReadMemory`、`WriteMemory` 更新记忆，写入遵循原会话权限。revision 防止网页和 Agent 并行修改时相互覆盖。

## 额度

省略 `limits` 时使用以下默认值，可按服务器规格调整：

```json
"limits": {
  "max_active_turns": 4,
  "max_active_per_user": 1,
  "daily_turns": 200,
  "daily_model_requests": 1000,
  "max_turn_seconds": 600,
  "max_prompt_bytes": 65536,
  "max_user_data_bytes": 8192
}
```

统计按 UTC 自然日重置。模型请求包含上下文压缩，一次逻辑请求的内部 HTTP 重试不另计次数，不能作为精确 token 或费用账单。重复消息不重复创建任务、扣除对话额度或执行工具。时限通过取消原循环并结算已有任务实现，结果未知时保留恢复状态。

额度不会隔离宿主内核，也不限制历史数据库总量。可另设 systemd 内存 / CPU 上限并检查状态目录空间；一个状态目录只能被一个云宿主持有。

## 可选：隔离服务器执行器

兼容的 `local_instance` 使用云服务系统账号执行工具，仅管理员可用。需要隔离工具与云端账号数据时，可替换为 `isolated_instance`。

在安装 Docker 的 Linux 服务器，复制预先构建的 Linux 执行器及配套脚本。先查看计划，再执行：

```sh
python3 scripts/provision-isolated-executor.py --binary ./peri-executor
sudo python3 scripts/provision-isolated-executor.py --binary ./peri-executor --apply
```

脚本要求已有 `/etc/peri-cloud` 和 `peri-cloud` 系统账号，拒绝覆盖既有实例。完成运输认证后，把 `/etc/peri-cloud/isolated-instance.json` 的字段合并进 `host.json`，移除 `local_instance`，校验再重启。脚本不改写宿主配置或自动重启它。

容器以 UID 10001 运行，根文件系统只读，只挂载 `/var/lib/peri-instances/owner/workspace` 和 `state`。默认 512 MiB 内存、1 CPU、128 个进程、64 MiB 临时目录，仅回环发布认证端口。内部网络不提供互联网访问，需要联网的管理任务可选择自己的外部实例。

容器的 `/` 是容器文件系统，只有 `/workspace` 对应宿主专用目录，不提供访问整个服务器的能力。云服务配置、模型密钥和历史不挂载。目录容量仍需文件系统配额或磁盘监控。

## 升级与回退

本次 schema 升至 v6。停服并等待任务结算，备份配置和一致的 SQLite 快照，再更换程序。既有 OAuth 设备、QQ 关联和历史不需重绑。原账号自动成为管理员，个人模型和记忆默认为空，继续使用服务器默认模型。

旧程序不能读取 v6。回退需同时恢复旧程序与升级前快照；新版产生的邀请、记忆、模型配置和任务不自动回灌旧快照。
