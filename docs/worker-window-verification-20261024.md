# 执行窗口接班验证 · 2026-10-24

范围：schema 26 的窗口意图、无工具摘要、当前约束锚点、继续策略及历史查询。仅使用模拟模型和临时目录中的真实本地执行器；没有调用真实模型，没有升级生产。Issue #7 按用户要求保持开放。

## 执行方式

Windows 原生验证，使用仓库固定 rmcp 补丁包装脚本；设置 `CARGO_INCREMENTAL=0`、`CARGO_PROFILE_TEST_DEBUG=0`、`CARGO_PROFILE_DEV_DEBUG=0`，构建并发为 1。不运行整个工作区全测。

```sh
./scripts/cargo-rmcp-patched.sh test --locked -p peri-cloud --lib -j 1 -- --test-threads=1
./scripts/cargo-rmcp-patched.sh test --locked -p peri-cloud --test gateway execution_session_tests -j 1 -- --test-threads=1 --nocapture
./scripts/cargo-rmcp-patched.sh test --locked -p peri-cloud --test gateway window_tests -j 1 -- --test-threads=1
./scripts/cargo-rmcp-patched.sh test --locked -p peri-cloud --test cloud_journal --test cloud_loop -j 1 -- --test-threads=1
./scripts/cargo-rmcp-patched.sh fmt --all -- --check
./scripts/cargo-rmcp-patched.sh clippy --locked -p peri-cloud --all-targets --no-deps -j 1 -- -D warnings
python scripts/check-upstream.py
```

## 结果

| 检查 | 结果 |
| --- | --- |
| cloud library | 134 通过 |
| 执行会话扩展回归 | 30 通过，1 个子进程入口按设计忽略；父测试显式启动该入口 |
| journal / 原循环 | 7 / 10 通过 |
| 隔离升级演练 | 已验证旧发布二进制 schema 9 → 26、候选重启、SQLite 备份完整性及恢复旧二进制；仅含合成账号/偏好数据 |
| 输出限制后的窗口专项 | 6 通过 |
| fmt / Clippy | 通过，Clippy 覆盖变更 crate 的 all-targets，警告视为错误 |
| 上游来源核验 | 1345 个文件：1300 相同，45 个允许的生产补丁 |
| 暂存差异敏感信息扫描 | Gitleaks 8.30.1 未发现泄漏 |
| 源/测试文件长度 | 本轮涉及的源/测试文件均小于 1000 行 |
| 可编辑拓扑 | 23 张图的节点/边引用与内联 JavaScript 语法检查通过；未把语法检查宣称为视觉验收 |

## 已验证场景

- 低预算下实际替换窗口，保留任务/逻辑执行会话 ID；可检索旧 Read 证据，不将旧窗口投射成普通聊天历史。三个连续主动换窗也能自动接续。
- 专用摘要没有工具；隐藏 reasoning 不进入交接输入。即使模拟提供商返回 Write 调用也没有执行器提交。
- 摘要超时、工具形响应及重启后的未知请求均降级为宿主事实；每意图一次逻辑请求，不反复整理。
- 真实子进程在等待、审批、原生提交、窗口准备和摘要请求在途时被强制终止；重启查询原身份和回执继续。摘要在途崩溃保留 `unknown_after_restart`，不再提交摘要。
- 手动换窗无关联主 goal 时停在 `window_ready`，普通聊天不会继续。显式关联活跃主 goal 时可续跑，数据库中不新增执行者 goal。
- 摘要期间主助手仍可处理补充要求与取消；新指令进入新窗的完整活跃快照，取消不会被旧摘要覆盖。
- 新窗必需契约放不下时等待，不循环切窗或消耗模型请求。
- provider 用量的向上校准和 Anthropic 缓存输入归一；未知用量不当成零。空检索页仍前移游标，原操作详情不能跨任务读取。
- 现有独立任务串行复用、不同执行会话并行、持久等待、自动审批、删除前通知、最新要求使旧验收失效继续通过。
- 原在途工具可跨窗运行/结算/取消；未知回执只查询不重放。主助手按调用固定设备，修改互斥与权限中间件保持有效。
- 真实遗忘、记忆到期和来源边界变化使旧执行上下文失效，不能靠交接重新带回旧隐私材料。

## 尚未进行

真实模型的完成质量、接续理解、重复阅读、检索回填效果、缓存命中率、实际延迟与费用均没有量化结论；没有生产数据库迁移、实际设备生产联调或发布升级。合成升级演练中的设备授权及历史表为空，不能替代真实生产数据恢复验收。词面检索不是语义搜索，预算估计不是 tokenizer，未激活的候选窗口暂不自动回收。详见 [运行契约](worker-window-runtime.md)。

提示词创作包保留架构、装配位置及创作指引；完成记录仅放在本验证文档，不混入待编辑正文。新可编辑拓扑 [worker-windows-20261024.html](design/peri-workbench/worker-windows-20261024.html) 使用独立浏览器存档键，旧画板和缓存不删除。
