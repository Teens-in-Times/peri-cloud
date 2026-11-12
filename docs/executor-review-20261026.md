# Peri 执行器设计审查报告(2026-10-26)

审查对象:`peri-executor` 及其执行链(`peri-tool-runtime`、`peri-process`、`peri-remote-tools`)。
基线:commit `867a4e4`,v0.8.0-rc.1。
方法:通读执行链全部非测试源码(~6k 行),逐项对照同类系统 reach-mcp v0.3.1 的已知问题模式(锁内 I/O、保留策略、版本护栏、进程归属、崩溃一致性、编码诚实性等)。参照系:Teens-in-Times/reach-mcp。

覆盖声明:已完整审查 peri-executor(config/web/service/shell/store/workspace/native/directories)、peri-tool-runtime 全部工具实现、peri-process/windows.rs、peri-remote-tools/session.rs。**未深入**:login/(OAuth 流程)、tunnel.rs/wireguard.rs(仅浏览)、environment.rs、mcp-packages/workspace 的 terminal/shell_tasks(本地实例路径,非远程执行器)。

---

## 一、总体评价

执行器的**核心可靠性设计是高水准的**,多处达到或超过 reach-mcp 的水准:

- **Windows 进程树归属完全正确**:`peri-process/src/windows.rs` 采用 CREATE_SUSPENDED + AssignProcessToJobObject + ResumeThread 的正确时序,并处理了 reach 都没有覆盖的边界:PID 复用校验(GetProcessId 对 retained handle)、挂起期多线程拒绝、`is_stopped` 用 Job 会计查询而非猜进程表、`disarm` 显式解除 kill-on-close。
- **不确定性协议严谨**:`peri-remote-tools/src/session.rs` 的 uncertain map 在 POST 前落账、404 不能 settle 已提交请求、ambiguous 错误分类(Disconnected/Protocol/408/5xx)、新调用被未决调用阻塞——这是"断连不重放"语义的正确实现。
- **幂等受理**:执行器按 `request_json` 全文比对去重,同 ID 不同参 conflict(`store.rs:97-112`)。
- **恢复语义诚实**:重启时 Queued→Cancelled、Running→RecoveryRequired;RecoveryRequired 阻塞该会话新工作直到澄清(`store.rs:211-239`、`store.rs:129-137`)——比 reach 更严格。
- **workspace 防护链完整**:device+inode 双因子身份、逐组件 symlink 检查(`workspace.rs:237-252`)、指令/技能读取的路径校验(`valid_skill_path`)。
- Grep 已具备并行遍历、确定序排序、协作取消、输出预算——这些是 reach 后来才补的,此处原生就有。
- sentinel 投影计数(防提示注入)与 Write 草稿恢复(from_draft)是两个有价值的设计创新。

以下发现按 **P0(正确性缺陷)/ P1(可靠性与资源)/ P2(加固与一致性)** 分级。每项给出位置、影响与建议修法。

---

## 二、P0 — 正确性缺陷

### P0-1 工具层存在双拷贝,且已发生行为漂移(执行器用的是坏的那份)

`mcp-packages/workspace/src/filesystem/` 与 `peri-tool-runtime/src/filesystem/` 是两套几乎完整的同名实现(read/write/edit/glob/grep/folder/draft/transaction),零交叉引用——因为二者挂在不同 BaseTool trait 上(`peri_agent::tools::BaseTool` vs `peri_acp_types::tools::BaseTool`)。

**已证实的漂移**:grep 分页 offset 语义。
- workspace 拷贝(`mcp-packages/workspace/src/filesystem/grep.rs:301-305`):"分页在结构化结果上完成:先丢弃前 offset 条,再对窗口应用 head_limit"——**正确**,且有专门注释说明;
- tool-runtime 拷贝(`peri-tool-runtime/src/filesystem/grep.rs:288-297` + `invoke` 末尾 `skip(offset)`):offset 在 head_limit 截断**之后**应用——**错误**。默认 `head_limit=250` 时,`offset>=250` 的第二页永远只剩截断标记,内容为空。**远程执行器走的是这份坏拷贝**(`peri-executor/src/native.rs:24-29` 直接构造 tool-runtime 的工具)。

这说明:同一个 bug 曾在一边修掉,另一边从未同步。只要双拷贝存在,以后每个修复都要人肉双写。

**建议**:
1. 短期:把 workspace 拷贝的分页修复(含 `collection_limit+1` 收集语义与 offset 提示)移植到 tool-runtime;
2. 中期:合并为单一实现。两条路:(a) 统一 BaseTool trait(动peri-agent,大);(b) 在 tool-runtime 实现纯函数核心(`execute_search`、`commit`、`read_lines` 等,不含 trait),两个 crate 各自薄封装成自己的 Tool 类型。**推荐 (b)**,改动局部、立刻消除漂移面。

### P0-2 Write 无法写空文件 / 无法截断文件

`peri-tool-runtime/src/filesystem/write.rs:197-199`:`content` 经 `filter(|c| !c.trim().is_empty() && *c != "__omit__")` 过滤,空串与纯空白被当作"未提供参数",落入 `Either 'content' or 'from_draft' must be provided` 报错。后果:无法用 Write 清空/截断文件(只能 Bash `Set-Content`/`> file`)。`content: " "`(有意写空白)同样不可能。workspace 拷贝 `write.rs:221` 同病。

**建议**:区分"字段缺失"与"字段为空"——`input.get("content").and_then(as_str)` 判存在性,`"__omit__"` 占位符逻辑保留;空串照常提交(commit 路径已能处理 pre→post 为空)。

### P0-3 Grep 的 glob 过滤只匹配 basename

`peri-tool-runtime/src/filesystem/grep.rs:166-171`:`p.matches(&file_name)` 只对文件名匹配。`glob: "src/**/*.rs"` 这类含路径的模式永不命中,且**无任何提示**,模型只会看到 "No matches found"。workspace 拷贝 `grep.rs:187` 同病。

**建议**:对 `strip_prefix(search_path)` 的相对路径匹配(globset 或双 Pattern 尝试:先相对路径,basename 作为回退),并在 description 里写明语义。

### P0-4(同源附带)Edit 的 `new_string == old_string` 未校验

schema 描述说 "must be different from old_string"(`edit.rs:127`),实现不检查,相同值会走一次完整读-改-写并报告成功。轻微,但属于"描述与实现不符"类。建议 invoke 开头补一行校验。

---

## 三、P1 — 可靠性与资源

### P1-1 无任何保留策略:jobs.sqlite 与 logs/ 无限增长

`store.rs` 没有任何 DELETE/清理路径;`shell.rs:36-47` 每次调用创建 `logs/{invocation_id}/stdout.log|stderr.log`,永不清除。长期运行的设备:数据库与日志目录单调增长;一条失控命令的日志可以把磁盘写满(见 P1-2)。

**建议**(参照 reach 的做法,量级可自定):
- 结束任务按条数/总字节/年龄保留(如 1000 条、512 MiB、14 天),过期先写 tombstone 状态再删日志目录;
- 单流日志上限(如 16 MiB,到达后继续读丢弃——reach 的做法:进程照常跑,落盘截断,回执标 `logs_truncated`);
- 启动时与定时(每小时)执行清理;清理失败不阻塞受理。

### P1-2 前台 Bash 在执行器侧无硬期限,配合 P1-1 构成无限资源占用

`peri-executor/src/shell.rs:111-113`:`deadline = background.then(parse_background_timeout).flatten()`——前台命令执行器侧无 deadline。云端等待超时后返回 `RunningAfterTimeout`("the command continues on the executor",`session.rs:439-449`),此后**没有任何机制终止它**:云崩溃、隧道断开、或模型忘记 cancel,进程与日志在用户设备上无限运行/增长。工具描述承诺的"同步路径恒有界"只在云端等待层成立。

**建议**:执行器对非后台 job 施加可配置的总运行硬上限(如 24h,或至少把 `timeout` 参数在 executor 侧同样 clamp 后生效为 deadline),到期 TerminateJobObject 并落 TimedOut;后台 job 也应有绝对上限或至少日志上限(见 P1-1)。

### P1-3 执行器私有状态对文件工具不设防

transport-token(明文,`config.rs:80-86`)、device.json、jobs.sqlite 都在 state_dir 下,而 Read/Grep/Glob 接受任意绝对路径且无拒绝列表(`tool-runtime/src/filesystem/mod.rs:27-44` 的 resolve_path 只做规范化)。模型一条 `Read("C:\\...\\transport-token")` 就能把运输令牌读进上下文(进而进入云端 trace/模型供应商日志)。Bash 本就全能,但文件工具的静默外泄更容易混过审批摘要。

**建议**(参照 reach 的 protected_paths):
- `NativeTools::new` 接收 state_root,构造拒绝列表(state_dir 全树 + 可执行文件自身);
- resolve 之后、执行之前统一校验,命中返回明确错误;
- 文档同时诚实声明 Bash 仍可读到(与 reach 相同的边界),此项是纵深防御而非沙箱。

### P1-4 提交原子性:tmp 写入无 fsync

`tool-runtime/src/filesystem/transaction.rs:44-66`:`std::fs::write(&tmp_path, post)` 后直接 rename,**没有 sync_all**,rename 后也没有父目录 fsync。断电/崩溃窗口内可能 rename 成功但数据未落盘,留下空文件或部分内容,且外表看是"已提交"。workspace 拷贝同病(其 transaction.rs:52)。

**建议**:`File::create` → `write_all` → `sync_all` → rename;Unix 上可选补父目录 fsync。Windows 分支的 rename 重试(5/32/33 退避)保留,那是好的。

### P1-5 无设备侧审计

执行器没有任何本地审计记录(只有 stderr tracing)。云端有 trace,但设备属主无法回答"这台机器上的执行器替谁在何时执行过什么、结果如何"。多账号云端 + 个人设备的场景里,设备侧审计是属主信任的基础。

**建议**:state_dir 下追加 JSONL 审计(session_id、invocation_id、tool、状态、时长;不含参数正文与输出);按大小轮转(如 5 MiB 保两代)。成本很低。

---

## 四、P2 — 加固与一致性

1. **web.rs 无限速/Host/Origin 校验**(`web.rs:42-61`):纯 Bearer、无 cookie,CSRF 风险确实低;但 SSH 反向转发把端口绑到 VPS 的 loopback 上,VPS 本地任何进程都可打。建议加简单 per-连接限速与 Host 白名单,与 token 现有常数时间比较搭配。
2. **persist_truncated_output 写系统临时目录**(`tool-runtime/src/output.rs:16-38`):Unix 默认 0644(其他本机用户可读,内容可能是代码/密钥匹配结果);文件永不清理,无限累积。建议 0600 + 启动时清理超过 24h 的 `peri-tool-output-*`。
3. **输出预算不一致**:Read 5,000B / Grep+Glob 20,000B / Bash 预览 5,000+3,000B / Bash output_char_limit 10,000。没有统一政策文档,模型难以形成稳定预期。建议集中为常量模块并写进架构文档。
4. **WriteSandbox 在 async 块内做阻塞 fs**(`write_sandbox.rs:356-399`):`tokio::time::timeout(async { std::fs::write... })` 阻塞运行时线程。执行器路径有 spawn_blocking 兜底,本地实例路径没有。移入 spawn_blocking。
5. **会话级 admission 全程持锁**(`session.rs:305`):`admission.lock()` 从受理持有到结果返回(最长 120s),只读工具也被串行。不确定性安全要求的是"受理边界互斥",不是"全程互斥"。建议:record_request 成功后即释放,结果等待阶段放行;或按工具只读/只写分级。
6. **directories browse 每页 O(N)**(`directories.rs:38-82`):每页全量读目录+排序再分页,10 万条目录翻页是 O(N²)。可接受但值得记录;上限 100_000 条的报错分支在循环内判 `children.len() > 100_000`,实际会先积累 100_001 条。
7. **Write/Edit 无版本护栏**:old_string 精确匹配是隐式保护,但 Write 全文覆盖对"模型拿旧缓存静默覆盖用户新改动"没有防御。draft 机制已经具备承载条件——建议加可选 `expected_sha256`(不匹配报 Conflict),Edit 可同样受益。
8. **Read 工具忽略取消令牌**:ToolContext.cancellation 已传入(`native.rs:122`)但 read/glob/grep 均不检查(grep 有自身 15s 超时)。长读无法被云端取消。低优先级。
9. **transport-token 在 Windows 上无 ACL 收紧**(`config.rs:115-127` 仅 Unix 0600):用户 profile 目录默认 ACL 尚可,但与 reach 的 icacls 收紧相比少一层。可选。

---

## 五、与 reach-mcp 的模式对照(可直接借鉴的清单)

| 主题 | reach-mcp v0.3.1 的做法 | peri 现状 | 建议 |
|---|---|---|---|
| 保留策略 | 128 条/256MiB/7 天 + 过期墓碑保指纹 | 无 | 采纳(见 P1-1) |
| 单流输出上限 | 16 MiB 截断 + `logs_truncated` 标志 | 无限 | 采纳(见 P1-1/2) |
| 私有路径防护 | protected_paths 拒绝列表(含自身 exe) | 无 | 采纳(见 P1-3) |
| 提交崩溃一致性 | before-image 日志 + fsync 后 rename;明确声明"非崩溃原子" | tmp 无 fsync,无声明 | 至少补 fsync(见 P1-4);是否上 journal 可另行评估 |
| 版本护栏 | replace/append/edit 强制 expected_sha256 | 无(隐式 old_string) | 可选采纳(见 P2-7) |
| 审计 | JSONL + 5MiB 轮转 | 无 | 采纳(见 P1-5) |
| 编码诚实性 | 无效 UTF-8 显式标志 + base64 无损通道 | from_utf8_lossy 静默替换 | 可选:预览保留 U+FFFD 但 evidence 加 `encoding_loss` 布尔 |
| 输出续读 | 字节游标 next_offset | 提示模型用 Read 读日志文件路径 | 现状可用;若做 P1-1 顺带做游标更优 |
| 交互能力 | keep_stdin 管道 + 完整 ConPTY 终端 | stdin=null,无交互 | 能力缺口非缺陷;若远程要跑交互式命令/TUI,可参考 reach 的 terminal 设计 |
| 进程归属 | 同样正确(suspend+job) | **更细**(PID 复用校验/会计查询/disarm) | peri 领先,保持 |
| 不确定性 | Unknown 状态 + 指纹幂等 | uncertain map + 404-不-settle | peri 领先,保持 |

---

## 六、建议的修复顺序

1. **立即**(小改,正确性):P0-2(空 content)、P0-3(glob 相对路径)、P0-1 短期(grep 分页修复移植到 tool-runtime)、P0-4;
2. **短期**(资源与防泄漏):P1-1 保留策略与日志上限、P1-2 前台硬期限、P1-3 私有路径拒绝列表、P1-4 fsync、P2-2(临时文件权限与清理);
3. **中期**(结构):P0-1 中期(工具层合并为单一核心实现)、P1-5 审计、P2-5 admission 粒度、P2-3 输出预算统一;
4. **随版本**:P2 其余项与 reach 对照表中的可选项。

---

*审查人:ZCode(GLM-5.3),2026-10-26。基于静态阅读与既有测试观察;未运行修改性验证。P0 项建议先补回归测试再修(grep 分页:offset=head_limit 应返回第二页;Write:content="" 应产生空文件)。*
