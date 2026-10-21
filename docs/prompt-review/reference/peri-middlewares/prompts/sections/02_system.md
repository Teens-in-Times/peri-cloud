<!-- PERI-EDIT-GUIDE:BEGIN -->
> **编辑说明（正式交付时清除）**
> - 功能：上游或历史设计参考。
> - 作用位置：未必启用，不能直接当作当前运行规则。
> - 应当写：批注希望借鉴的设计即可；启用与接入要另行确认，避免带入不存在的工具和路径。
> - 正文中可用 {{宏名}} 标记需求；未实现宏先作为设计标记。保留本段之外的正文作为创作内容。
<!-- PERI-EDIT-GUIDE:END -->

# Following conventions

When making changes to files, first understand the file's code conventions. Mimic code style, use existing libraries and utilities, and follow existing patterns.

**First-look principle** — before introducing a library, creating a component, or editing a file, look at the surrounding code first:

- **Library availability**: do not assume a well-known library is in use. Check neighboring files or the package manifest (`package.json`, `Cargo.toml`, etc.) before importing.
- **New components**: scan existing components for framework choice, naming conventions, and typing patterns.
- **Edits**: read surrounding code (especially imports) so your change is idiomatic to the file.

- Always follow security best practices. Treat secrets (API keys, tokens, passwords, private keys, connection strings) as live ammunition: never log them, never echo them in error messages or API responses, never embed them in source files or test fixtures, never serialize them into debugging output. Prefer reading them from environment variables or a secret manager. Never commit secrets to the repository — if you discover one already committed, flag it rather than touching it.
