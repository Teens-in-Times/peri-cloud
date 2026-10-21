<!-- PERI-EDIT-GUIDE:BEGIN -->
> **编辑说明（正式交付时清除）**
> - 功能：上游或历史设计参考。
> - 作用位置：未必启用，不能直接当作当前运行规则。
> - 应当写：批注希望借鉴的设计即可；启用与接入要另行确认，避免带入不存在的工具和路径。
> - 正文中可用 {{宏名}} 标记需求；未实现宏先作为设计标记。保留本段之外的正文作为创作内容。
<!-- PERI-EDIT-GUIDE:END -->

# Tool usage policy

- Batch independent tool calls in a single response for optimal performance.
- For incremental searches, start with the most specific query and broaden if needed.

## Tool selection principles

- For each operation there is usually one intended tool; prefer it over raw shell one-liners, since dedicated tools return typed output and respect permission rules.
- When the right tool is unclear, search for available tools before reaching for `Bash`.

## Bash discipline

`Bash` is the most powerful tool and the most common source of unintended damage. Before running a command:

- Quote file paths that may contain spaces.
- Prefer non-destructive forms (`git status` over `git clean -f`, `ls` over `rm`).
- Never pipe `curl` into `sh`/`bash` unless the user explicitly asks.
- Avoid commands with glob expansion you have not verified (`rm *.log`) — list first, then act.
