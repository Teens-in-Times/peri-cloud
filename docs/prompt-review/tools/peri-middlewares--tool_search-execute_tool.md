<!-- PERI-EDIT-GUIDE:BEGIN -->
> **编辑说明（正式交付时清除）**
> - 功能：工具使用契约。
> - 作用位置：工具目录的 description/schema。
> - 应当写：写何时使用、参数含义、返回值及失败处理；工具名与 schema 字段须和实现一致。
> - 正文中可用 {{宏名}} 标记需求；未实现宏先作为设计标记。保留本段之外的正文作为创作内容。
<!-- PERI-EDIT-GUIDE:END -->

# 工具说明与参数契约

正文为原始工具说明，参数部分为 Rust 生成表达式，字段身份与校验仍由代码保证。

## description · peri-middlewares/src/tool_search/execute_tool.rs:125

## parameters · peri-middlewares/src/tool_search/execute_tool.rs:129

~~~rust
fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "tool_name": {
                    "type": "string",
                    "description": "已注册的目标工具名称、大小写变体或声明的别名（例如 mcp__cron__cron_list）；以当前会话目录为准"
                },
                "params": {
                    "type": "object",
                    "description": "The parameters to pass to the target tool"
                }
            },
            "required": ["tool_name", "params"]
        })
    }
~~~

## prompt_declaration · peri-middlewares/src/tool_search/execute_tool.rs:117

~~~rust
fn prompt_declaration(&self) -> Option<String> {
        Some(
            "Invoke a registered deferred tool by name → `{{name}}` ({{title}}). \
             Use this after SearchExtraTools returns a tool name; call core tools in your tool list directly."
                .to_string(),
        )
    }
~~~
