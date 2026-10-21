<!-- PERI-EDIT-GUIDE:BEGIN -->
> **编辑说明（正式交付时清除）**
> - 功能：工具或中间件说明。
> - 作用位置：受实际工具与中间件装配开关控制。
> - 应当写：写使用时机、输入输出和限制；保持与实际能力一致。
> - 正文中可用 {{宏名}} 标记需求；未实现宏先作为设计标记。保留本段之外的正文作为创作内容。
<!-- PERI-EDIT-GUIDE:END -->

Performs exact string replacements in files.

Usage:
- You must use your Read tool at least once in the conversation before editing. This tool will fail if you attempt an edit without reading the file
- When editing text from Read tool output, ensure you preserve the exact indentation (tabs/spaces) as it appears AFTER the line number prefix
- ALWAYS prefer editing existing files in the codebase. DO NOT create new files unless explicitly required
- The file_path parameter must be an absolute path, not a relative path
- The old_string parameter must match exactly, including all whitespace and indentation
- The edit will FAIL if old_string is not unique in the file. Either provide a larger string with more surrounding context to make it unique or use replace_all to change every instance of old_string
- Use replace_all for replacing and renaming strings across the file

Error handling:
- old_string not found: returns an error indicating the string does not exist in the file
- old_string not unique: returns an error with the count of occurrences, suggesting more context or replace_all
- old_string is empty: returns an error rejecting the operation
- File not found: returns an error indicating the path does not exist
