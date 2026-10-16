# 个人 QQ 入口（OneBot v11）

Peri 的官方 QQBot 和个人 QQ 是同一个云助手的两个入口。官方适配器继续运行，个人入口单独配置、单独登录；双方共用原有 Gateway、账号、设备池、Agent 和工具审批。NapCat / LLBot 只是外部 QQ 协议端，不额外启动 AstrBot 的 Agent。

## 用户怎么理解

- 一个 Peri 账号可以关联多个聊天身份，各身份需要分别关联确认。
- 设备池属于账号；当前设备、工作区、权限和任务属于每条聊天会话。换一个群或入口，不会悄悄改变另一处的执行目标。
- 在群里 @ 助手交代任务；发送 `/菜单` 查看完整编号选项。日常群聊和其他机器人的命令不送进 Agent。
- 发送 `/选 2` 操作当前聊天中自己的最新菜单。引用原菜单并回复 `2`，操作被引用的那张菜单；仍检查发送者、群、有效期和会话版本。群友引用你的菜单不能代你操作。
- `/问题` 逐题回答和多选提交，`/详情 审批码` 查看完整参数，`/允许 审批码` 仅允许该次操作，`/拒绝 审批码` 拒绝。普通的“好的”不会代替审批。
- `/状态` 查看进度，`/取消` 请求停止，`/结果` 和 `/菜单详情` 阅读完整分页内容。无需为日常菜单、提问或审批另开网页。

首次账号与聊天身份关联沿用原有人工确认，不能按昵称或数字自动合并官方 OpenID 与个人 QQ 号。个人号登录扫码属于 QQ 的登录流程，不是执行器或 Peri 账号登录。

## 部署

在原有 `host.json` 顶层添加可选字段，原来的 `qq` 保持原样；不配置 `onebot` 时不增加监听端口，也不读取其令牌：

```json
{
  "onebot": {
    "instance_id": "qq-personal-main",
    "self_id": "123456789",
    "listen": "127.0.0.1:47672",
    "access_token_env": "PERI_ONEBOT_TOKEN"
  }
}
```

`instance_id` 必须与官方 QQBot 不同，绑定后保持稳定。`self_id` 是登录的机器人 QQ 号，必须是十进制字符串。令牌至少 32 个可见 ASCII 字符，由部署环境持有，不能放进模型工具参数。云服务先运行 `--check-config`，检查成功后再启动。

在 NapCat / LLBot 中创建 **OneBot v11 反向 Universal WebSocket**：

| 项目 | 值 |
| --- | --- |
| URL（同机） | `ws://127.0.0.1:47672/onebot/v11` |
| Access Token | 与 `PERI_ONEBOT_TOKEN` 相同 |
| 消息上报格式 | 数组（array） |
| 客户端角色 | Universal；同一条连接承载事件与 API |

反向连接校验 `Authorization: Bearer`、`X-Self-ID`、`X-Client-Role`，拒绝查询串令牌和第二条活跃连接。HTTP / Event / API 分离连接不属于当前支持范围。监听仅允许回环地址；远端协议端使用 SSH 转发或保留 Upgrade 与鉴权头的 WSS 反向代理。WireGuard 可承载 SSH 转发；不能把普通公网 `ws://` 当作加密连接。容器可通过受控的同机网络连接回环监听，协议端的管理界面也只向部署者开放。

部署者登录协议端后，用户在 Peri 账号生成关联口令，向个人号发送 `/绑定 口令`，确认新增身份，再发送 `/菜单` 选择实例。现有官方会话和设备选择不改变。

## 边界与恢复

适配器只发送规范化后的助手文字、通知和交互视图；外部事件、工具输出和内部推理不作为普通聊天直接转发。发送使用 text 消息段，模型生成的 `[CQ:...]` 不会变成平台指令。群回复引用原消息并 @ 实际发送者，以区分群内不同人的操作。

引用数字选择先调用协议端只读的 `get_msg`，核对回执消息 ID 和机器人作者，再将菜单快照 UUID 交给 Gateway 验证。引用关系不依赖内存缓存；服务重启后仍由持久发件箱和原菜单动作校验。未知、非机器人或跨路由引用不会退回模型执行。

入站按连接顺序提交原网关，平台消息 ID 持久去重。API `echo` 只关联回执，不是幂等键。收到成功回执且有 `message_id` 才记录投递成功；超时、断线、异步接受和不完整成功回执均保留未知，不在重连或重启后重发。服务停止会关闭并等待自己拥有的连接与入站工作，再排空原 Gateway / Agent / 执行器。

当前支持好友私聊、普通群文字、@ 和菜单引用；忽略自己的消息、匿名群消息、群临时私聊以及包含图片、文件、语音等不支持元素的消息，避免丢掉部分任务后误执行。文本里的 CQ 字符串格式不进行二次解析，请将协议端上报格式设为数组。当前未提供图片理解、个人号原生弹出按钮、合并转发报告或自动加好友。

个人 QQ 协议端独立部署，不随 Peri 分发；当前按自用场景接入。真实账号登录、客户端展示与稳定性需要实际扫码后验收，协议测试不能代替真实 QQ 验收。

协议依据：[OneBot 反向 WebSocket](https://github.com/botuniverse/onebot-11/blob/master/communication/ws-reverse.md)、[鉴权](https://github.com/botuniverse/onebot-11/blob/master/communication/authorization.md)、[消息事件](https://github.com/botuniverse/onebot-11/blob/master/event/message.md)、[公开 API](https://github.com/botuniverse/onebot-11/blob/master/api/public.md)。协议端安装见 [NapCat 官方指南](https://doc.napneko.icu/guide/boot/Shell)。
