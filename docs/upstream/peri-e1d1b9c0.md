# Peri v4 上游升级记录

## 上游

- 仓库：<https://github.com/KonghaYao/peri>
- 本发行基线：`d7ee444efe7a461b0696bd6c27c6c91f946fdec1`
- 升级目标：`e1d1b9c034092e4e58c163f1a8e5005a1db5f68c`（2026-10-17）
- 合并方式：文件叠加。本仓库与上游没有共同 Git 历史，不做 rebase/merge。
- 许可证：上游 Apache-2.0；本发行整体 AGPL-3.0-only，保留上游版权与 NOTICE。

## 叠加范围

从目标 commit 整体替换：

1. `peri-acp-types`
2. `peri-agent`
3. `peri-middlewares`
4. `peri-model`
5. `peri-process`
6. `peri-resources`
7. `peri-js-runtime`
8. `peri-workflow`
9. `peri-acp/prompts`（仅提示词；本发行不构建 `peri-acp` crate）

从目标 commit 新增：

1. `peri-config`
2. `peri-time`
3. `peri-mcp-core`
4. `mcp-packages/common`
5. `mcp-packages/config`
6. `mcp-packages/credentials`
7. `mcp-packages/web`
8. `mcp-packages/artifact`
9. `mcp-packages/cron`
10. `mcp-packages/workspace`
11. `mcp-packages/workflow`

删除：

1. `peri-lsp`
2. 工作区依赖 `lsp-types`

保持本发行版本，不覆盖：

1. `peri-cloud`
2. `peri-executor`
3. `peri-remote-tools`
4. `peri-tool-runtime`

## 升级前生产补丁清单（相对 `d7ee444e`）

按 Git blob 比较（忽略工作区换行差异）：

- 本发行跟踪文件：1144
- 上游基线文件：2149
- 修改：36（其中 Rust/Cargo 代码 29）
- 本发行新增：259
- 上游删除/裁剪：1264

### 修改文件

```text
.github/workflows/ci.yml
.gitignore
AGENTS.md
Cargo.lock
Cargo.toml
LICENSE
README.md
npm-packages/@peri-workflow/README.md
npm-packages/@peri-workflow/package-lock.json
npm-packages/@peri-workflow/package.json
peri-acp-types/Cargo.toml
peri-acp-types/src/interaction.rs
peri-acp-types/src/lib.rs
peri-acp-types/src/session.rs
peri-agent/Cargo.toml
peri-agent/src/agent/async_tasks/shell.rs
peri-agent/src/agent/compact_v2/full.rs
peri-agent/src/agent/model_bridge.rs
peri-agent/src/interaction/multiplex.rs
peri-js-runtime/Cargo.toml
peri-lsp/Cargo.toml
peri-middlewares/Cargo.toml
peri-middlewares/src/middleware/terminal.rs
peri-middlewares/src/tools/ask_user_tool.rs
peri-middlewares/src/tools/ask_user_tool_test.rs
peri-middlewares/src/tools/filesystem/mod.rs
peri-middlewares/src/tools/mod.rs
peri-middlewares/src/tools/output_persist.rs
peri-middlewares/src/tools/output_truncate.rs
peri-model/Cargo.toml
peri-model/src/openai_compatible/mod.rs
peri-process/Cargo.toml
peri-process/src/lib.rs
peri-resources/Cargo.toml
peri-resources/src/sessions/sqlite_store/discovery.rs
peri-workflow/Cargo.toml
```

### 新增（按顶层目录）

```text
1   .gitattributes
2   .github
1   NOTICE
1   THIRD_PARTY_NOTICES.txt
19  docs
2   examples
8   licenses
4   npm-packages
4   peri-acp-types
129 peri-cloud
29  peri-executor
1   peri-model
3   peri-process
8   peri-remote-tools
35  peri-tool-runtime
12  scripts
```

### 上游删除/裁剪（按顶层目录）

```text
1   .cargo
64  .claude
1   .dockerignore
3   .github
1   .gitmodules
1   .mcp.json
20  .peri
1   .typos.toml
1   CHANGELOG.md
1   CLAUDE.md
1   CONTEXT.md
1   Cross.toml
1   dev-acp.sh
1   dev.sh
65  docs
53  e2e
22  example
1   images
28  langfuse-client
1   lefthook.yml
15  npm-packages
155 peri-acp
2   peri-agent
53  peri-controller
1   peri-cool
27  peri-middlewares
5   peri-runtime
20  peri-theme
372 peri-tui
24  peri-web-pty
3   prompts
11  scripts
231 side-projects
76  spec
1   start-site.sh
```

`peri-acp` 的 155 个删除项是 `src`、`tests`、`Cargo.toml` 等；本发行只保留 `prompts`。`peri-middlewares` 的删除项主要是原生文件系统工具实现，本发行统一走 `peri-tool-runtime`。

## 生产补丁处置

| 补丁 | v4 处置 |
| --- | --- |
| `peri-acp-types` 设备接入/执行器协议与导出 | 保留并适配 v4；保留 `DeviceRecord.is_local` |
| `peri-acp-types/src/session.rs` 的 `TurnId::from_uuid` | 改用 v4 `with_turn_id`/`InvocationId`；无调用则删除 |
| 旧 interaction 模型、broker 旧签名 | 不移植；改用 v4 `TrustedInteractionOwner` |
| `peri-process/src/filesystem.rs`、`wireguard.rs` | 保留并挂回 v4 crate |
| `peri-model` user-agent、loopback `no_proxy`、`tests/client_identity.rs` | 保留并重放 |
| `peri-agent/src/agent/async_tasks/shell.rs` | 保留对 `peri-tool-runtime` 的委托导出 |
| `peri-agent` 旧 `summarize_context` | 改用 v4 `run_compact`/`CompactOutcome` |
| `peri-agent/src/agent/model_bridge.rs` 公共 provider API | 按 v4 公共签名适配 |
| `peri-middlewares/tools/filesystem/mod.rs` | 保留对 `peri-tool-runtime::filesystem::*` 的门面 |
| `peri-middlewares` terminal/output_* | v4 已删或迁移；确认无调用后删除 |
| `peri-resources/.../sqlite_store/discovery.rs` | 保留 `peri_process::filesystem::directory_identity` 取证路径 |
| `peri-executor`、`peri-remote-tools`、`peri-tool-runtime` | 整包保留，按 v4 类型适配 |

## 校验

`docs/upstream/peri-e1d1b9c0.manifest` 记录叠加范围内每个文件的上游 blob 哈希；`scripts/check-upstream.py` 校验本发行文件。允许清单内的生产补丁漂移，其余文件必须与上游一致。

Phase 1 叠加后校验结果：1264 个上游文件，1241 个与上游一致，23 个为允许清单内的生产补丁，0 缺失、0 未声明漂移。CI 在 Windows 任务执行 `python scripts/check-upstream.py`。
