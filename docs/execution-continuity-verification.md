# 完整架构验收记录

状态：完成原始完整目标的实现与本地验收（2026-10-19）。以下证据来自模拟模型与真实本地执行器，不使用真实模型密钥。提交及远端交付版本以仓库 Git 记录和交付消息为准。

## 需求与实际证据

| 目标 | 实现 | 验证证据 |
| --- | --- | --- |
| 身份、会话、任务、轮次、窗口分离 | schema 17–23；独立执行身份、执行会话与物理窗口，任务独立目标/约束/指令/状态/回执 | [execution_sessions.rs](../peri-cloud/tests/gateway/execution_sessions.rs) 的跨主会话连续任务与低预算窗口联调；[sessions_test.rs](../peri-cloud/src/tasks/sessions_test.rs) 的边界与迁移回归 |
| 显式复用与范围隔离 | OAuth principal、设备、规范工作区及文件系统身份、受众、隐私版本、所有来源代次；跨主会话须显式选择 | 同上；[execution_history.rs](../peri-cloud/tests/gateway/execution_history.rs) 验证复用前拒读、复用后只读旧任务、旧任务不可修改、无关联会话拒读 |
| 串行与并行 | 持久队首串行，同一逻辑执行会话不重复受理；Runtime 全局/用户额度限制独立会话并发 | [execution_parallel.rs](../peri-cloud/tests/gateway/execution_parallel.rs) 两设备同时达到真实执行断点，复用会话的第二任务保持排队 |
| 委派不阻塞聊天，主助手可直接执行 | 原主循环保留原生工具；授权发现、prepare_device、execute_device 固定按调用目标 | [execution_devices.rs](../peri-cloud/tests/gateway/execution_devices.rs)：两次准备后旧目标仍固定、无需委派写入、默认设备不变、前台聊天及另一设备可继续 |
| 主助手摘要压缩、执行窗口轮换 | 主 compact 保留；执行预算取宿主和模型容量较小值，预留续接空间；宿主事实与 agent 笔记分开 | 原 cloud_loop/上下文回归；execution_sessions 的低预算换窗、归档 Read 检索及迭代上限继续任务 |
| 旧工具跨窗口独立结算 | 原调用/窗口身份不变，接班材料只放引用与选定结果有界版本预览；waiting_operations 不冒充完成 | [execution_detached.rs](../peri-cloud/tests/gateway/execution_detached.rs)：后台 Bash 存活时多次换窗、检索原结果、宿主强杀后原绑定独立结算、单次取消 |
| 旧窗口无法总结也可恢复 | 宿主检查点及任务事实不依赖旧模型输出；新窗口从持久事实、笔记和有界证据启动 | [execution_crashes.rs](../peri-cloud/tests/gateway/execution_crashes.rs)：窗口准备未提交时真实强杀后恢复；低预算检索联调 |
| 历史有界、分页、筛选、详情 | list_tasks/related_tasks、窗口、消息、操作、审批、trace 与会话分页；明确 has_more/截断/保留边界；详情分段带版本 | execution_history 分页/授权；execution_sessions 与 approvals 的筛选、Unicode 详情及证据版本回归 |
| 不暴露隐藏思考 | trace 只收集工具、宿主事件及可见说明；会话及详情排除隐藏思考，保留原规范协议所需的私有存储边界 | 现有 gateway 隐藏思考投影回归；execution_sessions 原生输入详情与普通聊天均无 PRIVATE_REASONING |
| 持久暂停/等待/继续/取消 | 安全边界检查点、等待条件与唤醒；恢复核验当前授权、指令和旧回执 | execution_sessions：外部等待及暂停跨重启、父问题/消息唤醒；execution_crashes：等待中真实强杀；execution_detached：原操作取消 |
| 等待释放槽位、真实资源占用 | 外部等待释放推理槽位；审批等待 park，独立审批限流；获批后 waiting_capacity 重新取额度；未结算修改保留设备占用 | [execution_approval_capacity.rs](../peri-cloud/tests/gateway/execution_approval_capacity.rs)：仅一个后台槽位时独立任务能运行，获批任务必须等待额度；detached 等待仍保留修改占用 |
| 未知执行不重放 | 持久意图/受理/原始回执/对账证据；恢复只查原 invocation，404 不授权重试 | execution_crashes 实际 Write 已落地、HTTP 回执截留后强杀；执行回执缺失时零新增 POST，恢复可查后仍一次提交；execution_devices 跨设备恢复同样查询原目标 |
| 未知投递不重投 | 原发件箱状态权威，删除通知必须确认投递；未知不能另造通知绕过 | [execution_approvals.rs](../peri-cloud/tests/gateway/execution_approvals.rs) unknown_deletion_notice；现有 gateway 发件箱/QQ 投递回归 |
| 主助手独立有界自动审批 | 原 Agent loop 的一次无工具审查，独立于普通聊天；原 PermissionMiddleware 执行决定 | execution_approvals：完整参数、并行聊天、拒绝、新要求使在途决定失效、重启不复用决定；execution_crashes 审批中真实强杀 |
| 删除通知后自动执行 | 主助手语义审查加宿主保守识别；不新增人工确认；无法证明只读时提示可能删除/替换 | [execution_deletion.rs](../peri-cloud/tests/gateway/execution_deletion.rs)：echo 不多通知，嵌套实际文件删除先确认通知再执行；deletion.rs 覆盖展开、解释器与重定向 |
| 审批精确绑定与账号安全 | 用户、任务、执行会话/窗口、轮次、设备、完整参数、期限、指令水位；受理前再核验；原 OAuth/CSRF 保留 | approvals 失效/参数回归；README identity/native_login/portal/browser_approval/multi_user 原有安全套件 |
| 固定目标、统一操作与修改互斥 | 主原生、显式目标及工作者共用 cloud_native_operations；宿主连接配置不进入模型参数 | execution_devices：实际目标审批、无委派操作回执、旧目标查询、同设备修改拒绝及跨设备可用；原 transport_failover/执行器认证回归 |
| 新要求、取消及隐私始终有效 | 指令水位、每窗口重新完整阅读要求；来源代次、隐私版本与私人记忆有效期；不得借交接复活材料 | execution_approvals 新要求失效；execution_detached 的真实遗忘及记忆期限回归；sessions_test 边界拒绝 |
| 产物、验证、验收各自独立 | 宿主从本任务真实回执派生位置/版本，验证引用实际动作；最新要求使旧验收过期；无新增验证 agent | [execution_deliverables.rs](../peri-cloud/tests/gateway/execution_deliverables.rs)：实际 Write/Read、无证据拒收、有证据接受、追加要求失效、重启保留历史 |
| 任务动态与主助手插话 | 持久事件/双向消息、状态摘要帧、可配置本会话轮次刷新；无需工作者周期汇报 | [execution_observation.rs](../peri-cloud/tests/gateway/execution_observation.rs)：会话计数隔离、同回合工具迭代不重注入、跨重启保留游标 |
| 角色与续接规则 | main.md、worker.md、审批 REVIEW_RULES；直接/委派、要求优先级、恢复、证据规则；代码守住权限和生命周期 | 对应联调均经实际原循环、工具中间件和本地执行器，非仅提示词测试 |
| 既有模块与工程边界 | 账号/授权/主会话/记忆/调度/消息保持原入口；执行历史不自动写个人记忆；无 TUI 新依赖 | README 六 crate 套件；manifest/锁文件/许可证/NOTICE 与任务开始基线一致；上游 overlay 检查 |
| 可编辑拓扑与用户稿 | 保留用户原稿、Grok 参考、旧浏览器草稿；新增当前 schema 23 独立快照入口 | verify-atlas.cjs、verify.cjs、verify-review.cjs 检查层级、跳转、编辑/导入导出和旧存档保留；未做实屏截图检查 |

## 验证命令

全部 Rust 检查经 scripts/cargo-rmcp-patched.sh 应用固定 rmcp 补丁：

```sh
scripts/cargo-rmcp-patched.sh fmt --all -- --check
scripts/cargo-rmcp-patched.sh test --locked -j 1 -p peri-scheduler -p peri-cloud -p peri-executor -p peri-remote-tools -p peri-tool-runtime -p peri-process
scripts/cargo-rmcp-patched.sh clippy --locked -j 1 -p peri-scheduler -p peri-cloud -p peri-executor -p peri-remote-tools --all-targets --no-deps -- -D warnings
python scripts/check-upstream.py
```

最终六 crate 共 508 项通过、0 失败；格式检查及 peri-cloud / peri-remote-tools 两个变更 crate 连同 README 指定的 peri-scheduler / peri-executor 全目标严格 Clippy 均通过。三套工作台检查通过，上游检查为 1291 项、1264 项原样保留、27 项允许补丁。新增及变更源码/测试均小于 1000 行；依赖清单、Cargo.lock、LICENSE 与 NOTICE 未改变。强杀测试的 child 入口按设计 ignored，由组合测试实际启动并强杀，不是遗漏场景。Linux PTY 专项在 Windows 为零项；未重复执行 Linux 平台测试。

## 实际限制

- 上下文预算为保守估算，不等同供应商精确 token 计数；固定预留与有界资料不能保证任意大单条输出都能完整塞入窗口，完整证据通过分页检索。
- 设备修改占用采用整台设备粒度，较工作区文件级锁更保守；原修改未知时宁可保留占用。
- 产物与验证证明原操作时的事实，不持续监控之后的外部文件修改；语义验收覆盖描述属于带来源的 agent 判断，不能替代实际执行证据。
- 原执行或投递仍不可查时保持未知/等待，不提供“强行算成功”的恢复捷径。
- 联调使用模拟模型与真实本地 Windows 执行器；不把模拟回答当作真实模型能力评测。本次没有生产部署或真实模型付费调用。
- 可编辑设计原稿中的候选接口不等于运行配置；以当前实现子图、源码和本验收记录为准。独立最新入口为 [execution-architecture-20261019.html](design/peri-workbench/execution-architecture-20261019.html)，不会清除原画板缓存。
