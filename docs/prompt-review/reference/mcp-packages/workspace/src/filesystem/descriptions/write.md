<!-- PERI-EDIT-GUIDE:BEGIN -->
> **编辑说明（正式交付时清除）**
> - 功能：上游或历史设计参考。
> - 作用位置：未必启用，不能直接当作当前运行规则。
> - 应当写：批注希望借鉴的设计即可；启用与接入要另行确认，避免带入不存在的工具和路径。
> - 正文中可用 {{宏名}} 标记需求；未实现宏先作为设计标记。保留本段之外的正文作为创作内容。
<!-- PERI-EDIT-GUIDE:END -->

Writes a file to the local filesystem.

Usage:
- This tool will overwrite the existing file if there is one at the provided path
- ALWAYS prefer editing existing files in the codebase. DO NOT create new files unless explicitly required
- The file_path parameter must be an absolute path, not a relative path
- Parent directories are created automatically if they do not exist

Notes:
- Uses atomic write (write to temp file then rename) to prevent data loss on crash
- Atomic replacement depends on directory permissions: on Unix, a writable parent directory can allow an existing read-only file (for example mode 0444) to be replaced. The target file's read-only bits alone do not prevent overwriting; platform ACLs and directory permissions still apply.
- NEVER create documentation files (*.md) or README files unless explicitly requested by the User
- Only use emojis if the User explicitly requests it. Avoid writing emojis to files unless asked
- For files longer than 200 lines, consider writing in chunks: use Write for the first chunk, then Write with append=true for subsequent chunks. This reduces context window consumption significantly
