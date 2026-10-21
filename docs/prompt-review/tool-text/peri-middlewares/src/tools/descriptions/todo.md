<!-- PERI-EDIT-GUIDE:BEGIN -->
> **编辑说明（正式交付时清除）**
> - 功能：工具或中间件说明。
> - 作用位置：受实际工具与中间件装配开关控制。
> - 应当写：写使用时机、输入输出和限制；保持与实际能力一致。
> - 正文中可用 {{宏名}} 标记需求；未实现宏先作为设计标记。保留本段之外的正文作为创作内容。
<!-- PERI-EDIT-GUIDE:END -->

Maintain a todo list for complex multi-step tasks. Call this to create or update your todo list with the complete current state. Each call fully replaces the previous list.

Usage:
- Use this tool when working on complex, multi-step tasks that benefit from tracking progress
- Each call sends the COMPLETE todo list — this is a full replacement, not a partial update
- Include ALL items in every call, not just changed ones
- Mark items as "in_progress" when starting work on them, and "completed" when done
- Keep descriptions concise but specific enough to understand at a glance

When to use:
- Use for tasks with 3+ distinct steps that require tracking
- Use when the user explicitly asks for a plan or task breakdown
- Do NOT use for simple, single-step tasks
- Do NOT use for tasks that can be completed in a single tool call

Status values:
- "pending": Not yet started
- "in_progress": Currently being worked on
- "completed": Finished successfully

requireCompletion (optional):
- Set `"requireCompletion": true` to require every item to be marked "completed" before you end the turn.
- While set, if you stop with unfinished items, the system will show the current todo state and continue the turn until you mark them completed.
- Omit the parameter on later updates to keep the previous setting; set it to false (or mark all items completed) to release the requirement.
