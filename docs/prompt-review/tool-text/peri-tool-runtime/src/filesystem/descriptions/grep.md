<!-- PERI-EDIT-GUIDE:BEGIN -->
> **编辑说明（正式交付时清除）**
> - 功能：工具或中间件说明。
> - 作用位置：受实际工具与中间件装配开关控制。
> - 应当写：写使用时机、输入输出和限制；保持与实际能力一致。
> - 正文中可用 {{宏名}} 标记需求；未实现宏先作为设计标记。保留本段之外的正文作为创作内容。
<!-- PERI-EDIT-GUIDE:END -->

A powerful search tool built on ripgrep. Supports full regex syntax (e.g. "log.*Error", "function\s+\w+"). Filter files with glob parameter (e.g. "*.js", "*.{ts,tsx}") or type parameter (e.g. "js", "py", "rust", "go"). Use output_mode to control result format.

Usage:
- Always provide pattern parameter
- Use glob parameter for file type filtering (e.g. "*.js", "*.{ts,tsx}")
- Use type parameter for language-based filtering (e.g. "rust", "js", "py")
- Supports full regex syntax — literal braces need escaping (use \{\} to find interface{} in Go code)
- Output includes line numbers by default (use -n to disable)
- Search times out after 15 seconds; cancellation is cooperative: the walk stops at file boundaries, but a file already being scanned runs to completion (results are discarded) — use more specific patterns for large codebases
- Default head_limit is 250 output lines (matched + context lines); exactly N matches are NOT marked truncated, only outputs beyond N are truncated and persisted
- In files_with_matches / count / files_without_matches modes, head_limit limits the number of files listed (each file counts as one output line)
- Lines longer than 1000 bytes are truncated at a UTF-8-safe boundary with a "… [line truncated]" marker
- Total output is capped at 20000 bytes; beyond that the full output is saved to a temp file (use Read tool to view) and only the head is returned inline
- Use fixed_strings (-F) to search literal strings without regex interpretation
- Use invert_match (-v) to find lines that do NOT match the pattern
- Use whole_word (-w) to match whole words only
- Use multiline to match patterns spanning multiple lines
- Use max_depth to limit search directory depth

Output modes:
- "content": shows matching lines with line numbers (default)
- "files_with_matches": lists only file paths that contain matches
- "count": shows match counts per file
- "files_without_matches": lists only file paths that do NOT contain matches

Context control:
- -C: symmetric context lines before and after each match
- -A: context lines after each match (takes priority over -C)
- -B: context lines before each match (takes priority over -C)

When to use:
- Prefer Grep over Bash commands like grep or rg for content search
- Use Glob for file name search, Grep for content search
- For open-ended searches, start with the most specific query and broaden if needed
