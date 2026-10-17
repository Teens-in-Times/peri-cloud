# 控制接口、独立部署与恢复

本项目提供 Agent 主控和执行器，不内置、不代管 New API 服务。东京实例对接了已有的外部 New API；其他部署可直接填写自己的 URL、协议、模型与 Key，不需要部署聚合网关。两种模式都不依赖 LLM 完成配置管理。

## 两种部署模式

### 独立直连（默认）

`examples/host.json` 默认没有 aggregation。可省略该字段或设为 null：

```json
{
  "model": {
    "protocol": "openai",
    "api_base": "https://provider.example/v1",
    "api_key_env": "PERI_MODEL_API_KEY",
    "model": "your-model-id",
    "context_window": 131072,
    "max_tokens": 8192
  },
  "aggregation": null,
  "model_providers": []
}
```

这里是可合入完整 host.json 的配置片段，不是完整启动配置。`protocol` 省略时保持旧版 OpenAI 行为；目前支持 `openai`（Chat Completions）和 `anthropic`（Messages），不虚报 Responses 或其他协议支持。Anthropic 的 api_base 填服务根地址（例如 https://api.anthropic.com），适配器追加 `/v1/messages`；OpenAI 填包含 `/v1` 的基址。

管理员也可在“模型与 API → 自定义地址 / 协议”填写自己账号使用的端点、协议、模型与 Key。保存直接写入后端配置，不请求模型；Key 不回显。变更端点或协议须重填 Key，不能悄悄将旧凭据转发到新地址。普通成员可以选择部署者预设的供应商并保存自己的 Key；新增任意服务器访问地址只允许管理员，避免成员将模型配置当作内网请求代理。

部署者预设供应商：`model_providers` 每项包含 id、name、api_base 和可选 protocol。旧配置仍默认 openai。聊天 `/模型 模型名` 保持可用；初次录入 Key 在网页完成，不接受聊天明文密钥。

### 对接外部 New API（可选）

如果已有兼容的 New API，追加：

```json
{
  "aggregation": {
    "origin": "http://127.0.0.1:3000/",
    "management_key_file": "/etc/peri-cloud/credentials/management.token",
    "inference_key_file": "/etc/peri-cloud/credentials/inference.token"
  }
}
```

这是可选的服务端管理适配，不会安装聚合服务、迁移其数据库或向执行器下发上游 Key。当前管理适配只允许固定回环地址；跨主机网关可由部署者提供受保护的本地转发，或采用直连模式配置其推理 URL。兼容性依据 Tokyo 的 v1.0.0-rc.41-meridian-receipt1；其他 New API 版本需先核对 PAT 范围和渠道契约。

推理凭据只负责 `/v1` 推理/目录和自身用量。管理 PAT 仅授予 `channel:read`、`channel:operate`、`channel:write`、`channel:sensitive_write`、`group:read`，不授予 `secret_view`、账号、钱包或系统配置权限。凭据文件目录建议 root:服务组 750，文件 640；私人备份 600。源站 Key 只由 New API 保存，浏览器仅能提交新 Key，不能读取已有 Key。

该模式的服务模型入口是 origin + `/v1`。运行中的任务冻结本轮选择；后续轮次使用“会话覆盖 → 账号默认 → 服务默认”。切换工作区产生新会话时继承该会话显式模型选择，不影响其他聊天。关闭 aggregation 并重启可以恢复原有直连模式；不会删除旧个人模型记录。

## 东京部署的已核实配置依据

复用 www 站点、Nginx、TLS、现有 systemd 主控与外部 New API。根站的既有 Meridian 业务保留。Peri 自身仍只有一个主控进程和现有 SQLite；没有新增 Redis、队列或数据库服务。

New API 专用普通账号 `peri-service`、专用 `peri-internal` 组、有限调用额度 25,000,000 New API 单位；不是上游真实账单或免费额度承诺。不使用零倍率。新迁入的 DeepSeek 采用显式内部计量权重 1（输入/输出），只用于个人内部限额，不能作为供应商价格展示。既有模型计量未改。

已有 GPT、Claude 渠道只增加 peri-internal 组，原组保留；CommandCode 新增独立内部渠道，原始模型 ID 保持。已用统一凭据实际读取目录、用量并完成 DeepSeek 推理；其他模型及跨渠道故障转移不据此视作已验收。

推理 Key 仅允许回环和实际 Docker 入口桥地址。当前 Docker bridge 为 10.80.0.1，网络重建后应核对来源地址再调整；不要为恢复连通而清空 IP 限制。New API 现有公共入口与权限由其自身管理，Peri 不自动开放新监听。

## 正式控制接口

所有 `/api` 业务资源需要浏览器 Cookie 认证；POST 同时检查 Host、Origin、CSRF 和资源归属。管理员渠道操作另查管理员身份。OAuth/原生设备入口仍采用原有独立认证。

| 方法与路径 | 作用 / 权限 |
| --- | --- |
| GET /api/account | 当前账号、服务、可见入口与设备；不是在线证明 |
| GET /api/workspaces | 账号当前会话绑定及可切换工作区 |
| POST /api/workspaces/open | 用 request_id、device_id、workspace 创建独立网页工作区；远端验证路径 |
| POST /api/workspaces/:session/switch | 复用既有选择服务，切换指定会话目录 |
| POST /api/conversations/:session/tasks | request_id、confirmed、prompt；复用 IM 任务准入和原 Runtime |
| POST /api/conversations/:session/cancel | turn_id、confirmed；请求取消，不预报已停止 |
| GET /api/conversations | 当前绑定、任务阶段、本轮模型快照与后续模型选择 |
| GET /api/conversations/:session/result | 已存结果及 turn_state，不重新执行 |
| GET /api/interactions；POST /api/interactions/:id/respond | 共同 InteractionHub，精确请求归属和一次性审批 |
| POST /api/instances/:id/connection-check | 只读运输认证，不运行工具或调用模型 |
| GET/POST /api/model；POST /api/model/reset | 直连模式的账号配置；自定义端点/协议仅管理员可新增修改 |
| GET /api/aggregate | 是否启用外部集成、目录与默认模型；目录失败单独标注 |
| GET/POST /api/aggregate/model?session=UUID | 账号或指定会话选择，revision 防覆盖；不调用推理 |
| GET /api/aggregate/channels?page=N；GET /api/aggregate/groups | 管理员读取经过脱敏的渠道与分组 |
| POST /api/aggregate/channel-command | 管理员固定 create/update/status/delete/test；request_id、confirmed、配置 revision |
| GET /api/aggregate/usage；GET /api/aggregate/audit | 管理员读取网关额度、最近 50 个控制记录；缺失数据不估算 |
| GET/POST /api/admin/gateway-access | 原白黑名单服务，保留私聊 /网关 入口 |

渠道编辑支持真实模型列表、名称映射、分组、优先级、权重及 Key 替换。只显式测试才调用模型；测试可能消耗额度。启用只是配置状态，不能当作可用性。写超时或 5xx 进入 unknown；同 request_id 不自动重发。外部 New API 无跨进程 CAS，Peri 只能防自己页面覆盖已读取版本，不能承诺全局事务。审计保留操作 ID、发起人、目标、状态与时间，不保存提交 Key 或原始上游错误。

QQ 的原菜单、连接、工作区、权限、审批、问答、名单、个人记忆/偏好、取消与结果命令保留。QQ 和网页可操作同一条 QQ 会话；网页独立工作区属于另一条会话，不能谎称自动同步到任意群。菜单可带 `?session=UUID#activity-panel`，只含资源 ID，登录后再次授权，无秘密 URL。网页停用不影响原 IM 管理能力；LLM 不可用不影响这些确定性配置操作。

## 当前执行权限的信任边界

按现有授权，东京和北京执行器使用宿主机 root，本地使用当前 Windows 账号，并保留既有工具权限。这是可信个人管理员的执行资源，不能视为不可信多租户沙箱。服务文件 ACL、独立管理 PAT、浏览器和模型参数脱敏能阻止默认凭据下发，但不能阻止已获全工具 root 权限的任务读取同机服务文件。若以后接入不可信成员，应给这些成员配置自己的受限执行器或复用现有隔离实例机制，不能直接开放管理员 root 设备池。本次不擅自改变已授权的执行范围。

## 数据迁移与升级

schema 7 → 8 是一个 SQLite 事务，只添加 model_selections、control_operations、turn_models、model_endpoints 四张表，不改原身份、设备、会话、任务、个人 Key 或网关策略表。现有 journal 和设备标识保留。任务模型快照从本版本开始记录；旧任务显示“未记录”，不能根据现在的选择回填历史。

升级步骤：

1. 保存旧 release 路径、host.json、service.env；凭据副本仅放受保护的备份目录。用 SQLite backup API 创建一致副本，不能只复制有 WAL 的主文件。
2. 验证原生包 SHA256、BUILD.json commit、平台签名头及二进制版本；以服务账号执行 --check-config，并单独核验凭据文件可读。
3. 暂停入口准入并确认没有 queued/running/recovery_required 轮次和未确认投递。优雅停止主控，检查退出状态，再做最终数据库备份。启动失败的自动回退只能发生在消息入口尚未启用且未接收业务的验证阶段；最终启动后有新业务时采用前向修复或恢复直连配置。
4. 切换 release 符号链接和受保护配置，启动主控。检查 healthz、HTTPS、未登录 401、网页静态资源版本、原 QQ/OneBot 连接、设备运输身份及统一模型出口。
5. 放开准入，保留旧 release 和备份。执行器协议未改，可先保持原版本，逐台升级时保留设备状态和运输令牌。

--check-config 只验证结构，不读取凭据、不证明上游连通。模型不可用时主控依然能启动并登录管理；这有启动进程测试覆盖。

## 恢复与排障

- **启动失败且尚未接收新业务**：停止失败进程，恢复成对的旧配置、数据库 backup 和 release 链接，再启动旧版。迁移事务失败不会留下半套 schema；先保留失败数据副本排查。
- **已接收新业务**：优先前向修复；不能直接覆盖旧数据库备份抹掉新消息、审批或任务。旧 v0.5.0 只接受 schema 7，禁止直接拿它打开 schema 8。需要降级时先停机完整保存当前状态，明确导出/保留新增数据并做兼容演练；不要自动降低 user_version 伪装兼容。
- **仅外部聚合故障**：管理页面不需要 LLM；可检查渠道、凭据、有限额度、模型映射、分组。必要时恢复旧直连 host 配置和受保护 Key 副本，使用新主控继续运行，保留 schema 8 和所有新任务。
- **New API 配置撤销**：按本次变更清单撤销内部 Key/PAT、禁用新增渠道、移除仅本次新增的组和计量键。对既有渠道只移除新增的 peri-internal，保留其他人后续修改；不能恢复整个聚合数据库覆盖其他业务。东京原始选项、渠道投影和授权 ID 位于受保护备份 newapi-upgrade-20261017。
- **任务待核实**：先读任务 ID、执行器 journal 与恢复状态，不重复提交 prompt。关闭浏览器不等于取消，取消请求也不等于停止。
- **QQ 异常**：官方 WebSocket ready 与 OneBot 桥鉴权分开观测；桥连接不代表个人 QQ 已登录。原始平台载荷、Key、Cookie 不展示给浏览器。

自动验证：Windows cargo test/fmt/clippy、两个实际嵌入脚本 DOM 回归；Linux 只构建和启动/认证 smoke。隔离执行器测试断言审批前无写入、批准后真实文件存在、重复提交不重复执行、其他账号不能提交/审批，并验证 QQ 取消网页对同会话发起的任务。真实手机渲染和新增平台适配器仍需独立验收。

QQ 原生 Slash 命令、统一命令发现/执行 API、服务器注册接口和原生兼容矩阵见 [QQ CLI 命令](qq-cli-commands.md)。命令接口不会开放任意 Shell，也不代替原工具审批。
