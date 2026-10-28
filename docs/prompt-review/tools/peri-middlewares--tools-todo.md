<!-- PERI-EDIT-GUIDE:BEGIN -->
> **编辑说明（正式交付时清除）**
> - 功能：工具使用契约。
> - 作用位置：工具目录的 description/schema。
> - 应当写：写何时使用、参数含义、返回值及失败处理；工具名与 schema 字段须和实现一致。
> - 正文中可用 {{宏名}} 标记需求；未实现宏先作为设计标记。保留本段之外的正文作为创作内容。
<!-- PERI-EDIT-GUIDE:END -->

# 工具说明与参数契约

正文为原始工具说明，参数部分为 Rust 生成表达式，字段身份与校验仍由代码保证。

## description · peri-middlewares/src/tools/todo.rs:139

## parameters · peri-middlewares/src/tools/todo.rs:143

~~~rust
fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "requireCompletion": {
                    "type": "boolean",
                    "description": "Require every item to be marked status=\"completed\" before you end the turn. When set, if you stop with unfinished items, the system injects the current todo state and asks you to mark them completed. Omit it when updating the list to keep the previous setting; set to false (or mark all items completed) to release"
                },
                "todos": {
                    "type": "array",
                    "description": "The complete todo list (replaces all previous items). Include ALL items in every call, not just new or changed ones. Items not included will be removed",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": "A concise description of the task to be done (1-2 sentences)"
                            },
                            "activeForm": {
                                "type": "string",
                                "description": "Present-tense form of the task description (e.g. 'Running tests'), used for UI spinner display"
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                                "description": "Current status: 'pending' (not started), 'in_progress' (actively working), 'completed' (done)"
                            }
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }
~~~

## prompt_declaration · peri-middlewares/src/tools/todo.rs:132

~~~rust
fn prompt_declaration(&self) -> Option<String> {
        Some(
            "Maintain a visible task list → `{{name}}` ({{title}}) to track multi-step progress and cut context sprawl. Update it for any task with 3 or more distinct steps."
                .to_string(),
        )
    }
~~~
