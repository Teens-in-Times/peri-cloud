# 每日上游同步候选

用户选择：每天检测、尝试同步并开 PR，人工决定合并。本流程不自动批准、合并 main、升级生产或调用模型。

路线图索引：[Issue #13](https://github.com/Teens-in-Times/peri-cloud/issues/13)；实施待办：[Issue #12](https://github.com/Teens-in-Times/peri-cloud/issues/12)。

## 运行方式

- 工作流：`.github/workflows/upstream-sync.yml`，每日 UTC 01:17，即北京时间 09:17。GitHub schedule 为尽力调度，不能当作精确计时器。
- 支持手工运行；`dry_run` 默认 true，仅生成差异报告和 patch artifact，不写分支、Issue 或 PR。
- 固定来源 `KonghaYao/peri` 的 main。完整 revision 与 manifest 路径保存在 `overlay.json`；范围仍由 `check-upstream.py` 的 SCOPE 定义。
- 上游只改变范围外代码时记录情况，不创建只改 pin 的日常噪声 PR；以后有范围内变化时再从既有基线三方比较。

“无冲突”只表示确定性的文本合并成功。任务恢复、权限、工作区和提示词语义仍须人审；测试通过也不能替代这一步。

## 数据流与权限

1. **只读准备任务**：检出本仓库固定提交，不保存 checkout 凭据；仅 fetch 上游对象，不 checkout 或运行上游代码。先核对旧 manifest、祖先关系和本地漂移，计算全部结果，发生冲突时不部分写入。生成带 SHA-256 的 patch 和结构化报告。
2. **独立发布任务**：只在 main 的 schedule/manual 事件且非 dry-run 时运行。使用受信 main 脚本，核对 artifact、来源提交、路径、文件模式及 patch 摘要，仅向不可变候选分支提交。此任务拥有仓库内容、Issue、PR 与 Actions 写权限，**不运行 Cargo、npm、上游脚本或模型**。
3. **独立候选 CI**：发布草稿 PR 后显式 dispatch 原 CI，由只读 GitHub-hosted runner 构建和验证候选。构建任务不保存 checkout 凭据；需要认证的升级基线在执行候选代码前取得。自动候选不回写共享 Rust cache，不访问生产状态或凭据。
4. **人工决定**：即使 CI 全绿，仍保留草稿。维护者审查行为、依赖和文档后决定如何合并。

GitHub 对 `GITHUB_TOKEN` 创建的 PR 可能要求额外批准 workflow；token 的 push 也不会像人工 push 一样自动触发普通 CI。因此这里显式使用 `workflow_dispatch`，并保留独立运行记录。见 [GitHub Token 文档](https://docs.github.com/en/actions/concepts/security/github_token)。

写权限只存在于发布 job。工作流不使用 PAT、不读取仓库 secrets，也不提供 `pull_request_target` 执行候选代码的路径。它仍是供应链入口：恶意或错误的上游构建脚本可能破坏该次隔离 runner / 构建产物，不能将 CI 产物当生产发布物直接安装。

## 保留与停止规则

- 保留已登记的本地补丁；不自动扩大补丁白名单，不自动改根依赖约束、锁文件、rmcp 补丁、Cloud / Executor / Scheduler 实现或 CI 脚本。
- 只额外更新来源元数据：`overlay.json`、新 manifest / 同步说明，以及 README / AGENTS / NOTICE 的来源定位。历史记录保持原样。
- 文本、二进制、修改/删除或文件模式冲突、上游历史改写、许可证/归属变化、路径越界、symlink/submodule、大小超限或 Rust 文件达到 1000 行时停止自动发布源码。可分类冲突生成报告，无法安全读取或验证的输入使准备任务明确失败；两种情况均不发布部分源码。
- 上游新增文件撞上本地未跟踪/忽略文件时停止，不能把本地数据纳入候选。
- Cargo 依赖改变可能需要人工适配。保持 `--locked`，失败留在草稿，不靠自动改锁“修绿”。
- 同时只保留一个待审自动候选；不反复改写正在评审的分支。分支包含上游前后 revision 及本仓库基线身份，禁止 force-push。
- 同一候选被关闭后不重开。旧分支内容被修改或删除时不覆盖/复活；失败的原 CI 通过 CI 的显式 rerun 处理，不每日重复派发。
- 冲突 Issue 按上游前后完整 SHA 去重。历史分页最多 1000 项；超过预算明确失败，不能以截断列表证明不存在旧记录。
- main 在准备期间前进时跳过这份候选，下次重新计算。

所有检查都在仓库临时检出上运行。`prepare` 会修改并暂存候选源码，不应在有开发改动或生产文件的目录手工运行；脚本会拒绝脏工作区。

## 启用开关

GitHub 的 **Allow GitHub Actions to create and approve pull requests** 是同一个开关，启用它会同时放开创建与审批的仓库能力。本流水线不调用 review/approval/merge API。[GitHub 设置说明](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/enabling-features-for-your-repository/managing-github-actions-settings-for-a-repository)

2026-10-24 用户允许开启仓库级开关；首次操作被组织策略以 409 拒绝，当前凭据也无组织管理权限。用户随后处理组织开关，仓库设置成功读回 `default_workflow_permissions=read`、`can_approve_pull_request_reviews=true`；发布 variable 已设为 `true`。没有扩充本机凭据 scope 或写入 PAT。

独立的仓库 Actions variable `PERI_UPSTREAM_PUBLISH` 必须精确设为 `true` 才会执行发布 job，未设置时仅检测和产出报告。只有核验上述权限后才启用此 variable；将其设为 `false` 可立即退回只读检测。该开关不影响手工 `dry_run=true` 的只读保证。

保持仓库默认 token 为 read，仅发布 job 明确请求最少必要写权限。停用每日任务可在 Actions 禁用 `Daily upstream candidate`；无需删除代码或重置数据。人工模式始终可先运行 dry-run 检查。

## 验证

本地使用临时 Git 仓库与 API mock，验证本地补丁保留、冲突不部分落盘、忽略文件/符号链接/非法路径保护、大小与 manifest 校验、patch 摘要与保护路径校验、原候选不覆盖，以及只推候选分支、只开草稿 PR 和触发 CI。工作流另经 actionlint 检查。

2026-10-24：8 项本地隔离测试通过，actionlint 1.7.12 检查两个工作流通过；来源校验为 1345 个文件、1300 个完全一致、45 个已登记补丁。实际 fetch/prepare 拉取固定上游，返回 `unchanged`，head 与 pin 均为 `e598d17872bce487355e1bfa70f077743984760f`。没有为本次 Python / 工作流改动重复运行整套 Rust 测试。

实际定时触发、GitHub token 开 PR 与首次新上游候选的端到端结果，应以 Actions 和 PR 记录为准；dry-run 成功不代表这些写入路径已验证。
