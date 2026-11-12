# 工具输出预算政策（单一事实源）

> 来源：executor 设计审查 2026-10-26 P2-3。此前各工具各自硬编码
> （Read 5,000B / Grep+Glob 20,000B / Bash 预览 5,000+3,000B /
> Bash output_char_limit 10,000），数值分散且无统一政策说明。现在全部集中在
> `peri-tool-runtime/src/budget.rs`，改预算只动那一个文件。

## 常量一览

| 常量 | 值 | 使用方 |
| --- | --- | --- |
| `READ_OUTPUT_BYTES` | 5,000 | Read 内联输出；超过按行截断并提示分段读取（Read 不落盘，避免二次编号） |
| `SEARCH_OUTPUT_BYTES` | 20,000 | Grep / Glob / folder_operations 内联输出；超过落盘（`persist_truncated_output`）+ 头部内联 |
| `BASH_STDOUT_PREVIEW_BYTES` | 5,000 | 执行器 Bash 回执的 stdout 预览 |
| `BASH_STDERR_PREVIEW_BYTES` | 3,000 | 执行器 Bash 回执的 stderr 预览 |
| `BASH_OUTPUT_CHAR_LIMIT` | 10,000 | Bash 工具描述符的 `output_char_limit`（宿主侧整体字符上限） |
| `BASH_LOG_STREAM_LIMIT_BYTES` | 16 MiB | 执行器单条 Bash 日志流（stdout/stderr 落盘文件）上限；到达后进程照常运行、落盘截断、回执标注（P1-1） |

## 政策

- **单行工具**（Read/Grep/Glob/Folder）内联输出用字节预算；超限截断/落盘提示。
  Grep 额外有 `MAX_LINE_BYTES = 1,000`（单行截断）与 `head_limit`（默认 250，
  行数预算）；分页在结构化结果上完成（offset → head_limit 窗口）。
- **Bash 回执**用 stdout/stderr 双预览 + 字符上限；全文走日志文件
  （`logs/{invocation_id}/stdout.log|stderr.log`，16 MiB/流封顶）。
- **截断结果**保存在上游进程私有目录中。Unix 目录为 0700，文件为 0600。清理保留持锁的活跃目录，移除超过 7 天的弃置目录，以及超过 24 小时的旧格式临时文件。
- 预览经 lossy UTF-8 解码产生替换字符时，回执 evidence 置 `encoding_loss: true`
  （编码诚实性，见 `ToolExecutionEvidence`）。

## 保留策略（关联，P1-1）

- 终态 job：最多 1,000 条 / 14 天（`peri-executor/src/store.rs`）；超限移入
  墓碑表，保留原调用 ID、请求指纹、实际终态及退出证据，丢弃正文和日志路径。原 ID 可继续通过 GET 结算。
- 日志目录随 job 删除；孤儿日志目录（无 job 行且超龄）启动与每小时清理。
- RecoveryRequired 状态不删除——它是会话级阻塞语义，删除会丢失恢复义务。
