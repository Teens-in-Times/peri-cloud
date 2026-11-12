# 执行器审查修复记录（2026-10-26）

对应审查报告（`docs/executor-review-20261026.md`）全部 P0/P1/P2 条目的落地说明。
基线 commit `867a4e4`（v0.8.0-rc.1）。

## P0 — 正确性

- **P0-1（双拷贝漂移）**：短期——`peri-tool-runtime` 的 Grep 采用 workspace 侧
  已修复的分页语义（收集上限 = offset + head_limit，分页在结构化结果上完成，
  不再对截断后的字符串 skip）。中期——按报告建议 (b) 落地：算法核心全部收敛到
  `peri-tool-runtime::filesystem`（`execute_search` / `commit_guarded` /
  `apply_edit` / `run_glob` / `plan_walk` / `list_folder` / `deep_scan_folder` /
  `render_lines` / `DraftStore` / glob 编译 / per-target 事务），本地
  `mcp-packages/workspace` 与远程 `peri-executor` 两侧只保留各自 BaseTool
  trait 上的薄封装与错误文案映射（ToolFailure/path 提示 vs 朴素字符串）。
  两侧 8 个工具族（read/write/edit/glob/grep/folder/draft/transaction）不再
  有第二份实现；此前两边各自漂移的消息文案（落盘提示、glob 早停计数）已统一。
- **P0-2（空文件）**：Write 区分「字段缺失」与「字段为空」——空串/纯空白是合法
  content（可清空、截断文件），仅 `__omit__` 占位符与字段缺失走缺参报错。
- **P0-3（glob 只匹配 basename）**：Grep 过滤器先匹配相对 search_path 的
  `'/'` 分隔路径，basename 回退；schema 描述同步写明语义。
- **P0-4**：Edit 在读文件前校验 `new_string != old_string`，相同值不再走完整
  读-改-写并谎报成功。

## P1 — 可靠性与资源

- **P1-1（保留策略）**：终态 job 1,000 条 / 14 天，超限移入 `tombstones` 表
  （保 id + 请求 SHA-256 指纹 + 终态快照）——幂等受理与 GET 结算在过期后仍然
  成立，同 id 异参仍按冲突拒绝。日志目录随 job 删除，孤儿日志目录（无 job 行
  且超龄）一并清理；启动 + 每小时执行，失败只跳过单个对象。单条 Bash 日志流
  经管道排水并以 16 MiB 封顶：进程照常运行，落盘截断 + 文件尾标记，回执文本
  标注 `[Log truncated on disk …]`（`store.rs`、`shell.rs`、`service.rs`）。
- **P1-2（前台硬期限）**：执行器对所有 job 施加总运行硬上限（默认 24h，
  `PERI_EXECUTOR_JOB_HARD_CAP_MS` 可覆盖，clamp [1min, 30d]），到期
  TerminateJobObject 并落 `TimedOut`；显式后台 timeout 取两者较小值
  （`shell.rs`）。云端「等待超时后继续跑」的 promote-to-background 契约不变，
  只是无限续跑的时代结束。
- **P1-3（私有路径防护）**：`NativeTools` 携带拒绝列表（state_dir 全树 +
  执行器自身 exe），`validate`（受理前）与 `invoke_file`（执行前）双重校验；
  命中返回 403 `ProtectedPath`。祖先语义：覆盖受保护根的目录搜索（如整盘
  Grep）同样拒绝。Bash 仍然全能——这是纵深防御而非沙箱，文档如实声明。
- **P1-4（提交原子性）**：tmp 写入 `sync_all` 后才 rename，Unix 上补父目录
  fsync；Windows 的 rename 重试保留（`transaction.rs`）。
- **P1-5（设备侧审计）**：`state_dir/audit.jsonl` JSONL（session/invocation/
  tool/状态/时长，不含参数正文与输出），5 MiB 轮转保一代，受理与结算各记一条
  （`audit.rs`）。

## P2 — 加固与一致性

- **P2-1**：web 入口加 Host 白名单（按监听地址推导，loopback 名 + 接口地址）、
  Origin 校验（跨源浏览器页 403）与每 IP 滑动窗口限速（600 req/10s，容纳协议
  自身 100ms 轮询）（`web.rs`）。
- **P2-2**：`persist_truncated_output` Unix 0600 创建；启动清理超 24h 的
  `peri-tool-output-*` 残留（`output.rs`、`service.rs`）。
- **P2-3**：输出预算统一到 `peri-tool-runtime/src/budget.rs`，政策见
  `docs/output-budgets.md`；执行器预览与 Bash 描述符常量改为引用。
- **P2-4**：WriteSandbox 的阻塞文件系统链移入 `spawn_blocking`，timeout 只
  约束 JoinHandle。
- **P2-5**：admission 锁只覆盖受理边界（uncertain 检查 → GET 预检 →
  record_request → POST），结果等待在锁外进行；不确定性安全由 uncertain map
  自身保证（新调用观察到未决 id 即拒绝提交），只读查询/取消不再被 120s 等待
  串行化（`peri-remote-tools/src/session.rs`）。
- **P2-6**：directories 100k 上限在 push 前判定（不再积累 100,001 条）；分页
  O(N)/页 的取舍记录在代码注释。
- **P2-7**：Write/Edit 增加可选 `expected_sha256`（64 位十六进制）；提供时不
  匹配当前文件即拒绝（`CommitError::ShaMismatch`），防「模型拿旧缓存静默覆盖
  用户新改动」。
- **P2-8**：Read/folder/Glob/Grep 接入宿主取消令牌（入口检查 + 阻塞循环内
  检查 + invoke future 丢弃时的 Drop 兜底），云端取消能真正停住本地扫描/读取。
- **P2-9**：Windows 上 transport-token/device.json 写入后用 `icacls` 去继承、
  仅当前用户完全控制（尽力而为，失败告警不阻塞启动）。
- **编码诚实性**（reach 对照可选项）：`ToolExecutionEvidence` 新增
  `encoding_loss`（serde default），执行器 Bash 预览在 lossy 解码发生时如实
  置位；保留 U+FFFD 的同时不再假装字节忠实。
- **输出续读**：维持「提示模型 Read 日志文件」的现状（报告判定现状可用），
  未引入字节游标。
- **交互能力**：报告判定为能力缺口而非缺陷，未在本次范围。

## 移植至 `41e0ed9`

- Workspace 的 Read 保留上游图片结果及原参数校验。Write/Edit 在共享事务锁内捕获上游回退所需的前后镜像，回退也经过同一把锁。Cloud 继续使用原生能力目录发现执行器工具。
- 终态压缩保留原调用身份、实际终态及可用的退出证据。原调用 ID 仍可通过 GET 查询，重提同一请求只返回旧回执。日志文件可由原执行会话通过精确路径 Read，其他文件工具继续受私有路径检查约束。
- 截断内容沿用上游进程私有目录和活跃进程锁。清理兼容升级前遗留的旧格式临时文件。HTTP 的 Origin 必须匹配请求的 Host 与端口，SSH 转发端口继续由连接配置决定。

## 原基线环境记录

`peri-executor/tests/http_execution.rs::shell_alias_captures_stdout_stderr_and_nonzero_exit_evidence`
在本机（非 CJK locale，PowerShell 控制台 OEM 编码无法表示 `你好`）于基线
commit 即失败——与本次改动无关，已在基线 worktree 验证复现。
