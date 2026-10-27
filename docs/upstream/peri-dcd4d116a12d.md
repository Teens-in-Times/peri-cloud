# Peri dcd4d116 同步与取消收尾

2026-10-26 人工处理 [Issue #18](https://github.com/Teens-in-Times/peri-cloud/issues/18)，将上游源码基线从 `199382c5d048ae033104ab64a9f63eba13fe6e87` 更新到 `dcd4d116a12dacdc8db41ec5d1387c6bbe874fdd`。

[上游差异](https://github.com/KonghaYao/peri/compare/199382c5d048ae033104ab64a9f63eba13fe6e87...dcd4d116a12dacdc8db41ec5d1387c6bbe874fdd)只有一个提交（上游 PR #292，修复 #291），范围内涉及三个 Rust 文件。每日同步正确拒绝了合并后达到 1000 行的 `run_sync_test.rs`，未发布部分源码；本次不修改自动化的停止规则。

## 采用的修复

- SDK 的同步子执行由独立 Tokio task 持有。父工具调用 future 被取消或丢弃后，取消守卫通知子执行，子执行仍走既有 Interrupted 收尾，完成历史 flush、资源关闭、状态落盘与停止事件。
- 完成合法后台交接后解除守卫；父调用结束不会误取消已经交接的执行。JoinError 显式映射为错误，原收尾屏障保留。
- 资源关闭失败时不提交可恢复的终态，不结清恢复 claim；后续事件排空也失败时，同样不得绕过该条件写终态。
- 采用上游取消回归用例，并将 `BlockingLlm`、对应 context builder 和取消测试原文移至 `run_sync_cancel_test.rs`。原测试文件增加子模块声明，测试内容及断言不变；所有相关源码与测试均少于 1000 行。
- 严格 Clippy 发现两处旧测试拆分遗留的悬空文档注释，将说明移回对应测试函数；只移动注释，不改测试行为或放宽 lint。

## 适配边界

`run_sync.rs` 与 `resume_dispatch_test.rs` 和新上游字节一致，仅为测试拆分新增一条生产补丁白名单。新 manifest 仍包含 1354 个上游文件：1304 个一致，50 个已登记差异；新增测试文件属于本地拆分文件。

这是继承 SDK 的同步子执行生命周期修复，不将 SDK 子执行接入 Cloud 的持久 Worker，也不替换 Cloud 的任务、轮次、窗口或原生操作账本。Cloud schema 仍为 26，根依赖、锁文件、rmcp 补丁、账号、审批、设备与工作区授权不变。上游 `spec/` 记录不在源码同步范围内，通过上述来源链接保留追溯。

## 验证

本地 Windows、模拟模型验证：

- 通过补丁包装脚本运行 `test --locked -p peri-agent --lib -- --test-threads=1`：1332 项通过、0 失败、4 项既有忽略。包含取消后恰好一次停止事件、恢复取消收尾、交接成功/失败与资源保留用例。
- 同一脚本运行 `clippy --locked -p peri-agent --all-targets --no-deps -- -D warnings` 及 `fmt --all -- --check`：通过。两处旧文档注释归位后，额外核对对应四个文件的非注释代码完全未变。
- `python -m unittest discover -s scripts/tests -p test_upstream_sync.py`：8 项通过。`python scripts/check-upstream.py`：通过。
- 核对新上游的实现和恢复测试原文一致；重组拆分文件可还原原测试文件，全部测试及断言保留。检查仓库 Rust 源码/测试均少于 1000 行，依赖、锁文件、许可证、固定补丁和云端/执行器实现均未变化。

没有重复运行整个工作区测试；GitHub CI 另以关联 PR 的精确提交为准。本地日志留在忽略的 `target/`，不作为源码提交。

未调用真实模型，未操作生产状态或部署。原开发工作区的未提交改动保留，本次在独立工作树中完成。
