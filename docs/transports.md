# 执行器连接与控制面迁移

设备是执行目标，线路是到达同一设备的方式。切换 SSH / WireGuard 不创建新设备，不改写会话、工作目录或任务 ID，也不改变工具审批策略。模型和 QQ 用户不需要选择网络线路。

## SSH 与 WireGuard 并存

默认仍只接受回环 HTTP。执行器可额外启用 WireGuard 私网监听：

```sh
peri-executor --state-dir ./state --listen 127.0.0.1:42371 \
  --wireguard-interface wg0 --wireguard-listen 10.66.0.3:42371 \
  --ssh-host cloud --ssh-reverse-listen 127.0.0.1:44371
```

两个监听器共用同一个执行器、设备 UUID、运输令牌和任务日志。WireGuard 适配器暂时不在线时，执行器重试额外监听，回环与反向 SSH 继续运行。现有 SSH 参数保持兼容。

云端设备条目保留 SSH 的回环端点，增加可选线路：

```json
{
  "device_id": "00000000-0000-0000-0000-000000000042",
  "endpoint": "127.0.0.1:44371",
  "token_file": "devices/transport-token",
  "wireguard": {"interface": "wg0", "endpoint": "10.66.0.3:42371"}
}
```

云端先验证本机 WireGuard 适配器、目标路由和私网源地址，再使用该源地址发起连接。仅填写一个普通内网地址不构成 WireGuard 信任；公网、通配地址、回环伪装及无效接口名会被拒绝。Windows 使用系统网络接口信息，Linux 使用 `ip -j`。不读取 WireGuard 私钥，不修改网络、路由或默认网关。

连接建立和重新选择设备时优先 WireGuard；网络不可达时尝试 SSH 回环端点。每条线路都必须通过运输令牌、设备 UUID、协议版本和平台校验。身份错误、认证拒绝或无效协议直接失败，不通过备用线路掩盖错误。

运行中的连接失效时可重新认证并切换线路。只自动重试 GET 查询；POST / PUT 的响应丢失后不会自动重发。调用方保留原任务的不确定状态，查询原任务 ID 后才能决定后续操作。新线路和旧线路上的任务、会话和工作目录身份必须一致。

Windows 常驻脚本支持 `executor.json` 中可选的 `wireguard_interface` 与 `wireguard_listen`。不配置这两个字段时继续使用原 SSH 行为。云端仍需配置设备的运输令牌；OAuth 登记成功不等于设备在线。

## 运维检查

```sh
peri-cloud-host --config host.json --check-config
peri-cloud-host --config host.json --check-devices
```

第一条只验证配置，不读取凭据。第二条只读取运输令牌并认证设备，输出设备 ID、平台和当前线路类型，不启动模型、QQ 或网页，不提交工具任务。SSH 转发必须已经启动；同宿主动态生成的兼容 `local_instance` 不在此静态设备探测范围内。

## 控制面迁移

HTTPS 网页、身份与会话状态、QQ 网关和 Agent 宿主属于控制面。Windows / Linux 执行器及其工作目录属于执行面。把主控迁到另一台服务器不要求移动执行面。

迁移按以下顺序进行：

1. 找到线上实际源码与数据库 schema；使用对应版本及其增量变更，避免旧写入器打开新 schema。
2. 在新主机准备独立系统账号、受限配置文件、正确的 HTTPS `public_origin` 和反向代理。域名与证书必须覆盖公开入口。
3. 使用一致的 SQLite 备份进行预演；预演停用 QQ，并使用独立状态目录及端口。验证账号数据、执行器认证和网页来源检查。
4. 保留原执行器 UUID、运输令牌、状态目录及工作目录。目录对象身份不能通过复制文件伪造；原服务器可作为远程执行器保留。
5. 等旧宿主无活动任务、未决审批或待投递回复后，停止旧宿主，复制最终状态，启动新宿主。QQ 同一时刻只保留一个所有者。
6. 更新反向 SSH 目标和本机登录入口，验证 HTTPS、QQ 会话连接及真实设备线路，再禁用旧宿主自动启动。

迁移前保留程序、配置及同一时刻的状态快照。新宿主开始接收任务后，回退必须先停止接收、交接最新状态；不能直接恢复迁移前的数据库，否则可能丢失会话或重复处理消息。

## 回归验证

```sh
cargo test --locked -j1 -p peri-process -p peri-executor -p peri-remote-tools -p peri-cloud
cargo clippy --locked -j1 -p peri-process -p peri-executor -p peri-remote-tools -p peri-cloud --all-targets --no-deps -- -D warnings
```

`transport_failover` 使用真实 HTTP 服务和主动丢失响应的代理，覆盖同设备读请求接续、认证拒绝不降级、失联 WireGuard 回退，以及写入已接收但响应丢失时不重放。日常测试不依赖开发者的 WireGuard 网络；部署时另做实际适配器与两条网络线路的验收。
