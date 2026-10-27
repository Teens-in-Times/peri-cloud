<!-- PERI-EDIT-GUIDE:BEGIN -->
> **编辑说明（正式交付时清除）**
> - 功能：工具使用契约。
> - 作用位置：工具目录的 description/schema。
> - 应当写：写何时使用、参数含义、返回值及失败处理；工具名与 schema 字段须和实现一致。
> - 正文中可用 {{宏名}} 标记需求；未实现宏先作为设计标记。保留本段之外的正文作为创作内容。
<!-- PERI-EDIT-GUIDE:END -->

# 工具说明与参数契约

正文为原始工具说明，参数部分为 Rust 生成表达式，字段身份与校验仍由代码保证。

## description · peri-cloud/src/archive/tool.rs:15

~~~text
Read original archived context by its reference, with a bounded token budget and continuation cursor. Fragments use byte offsets and declare utf8 or hex encoding; complete and next_cursor describe completeness. References do not grant access to other sessions.
~~~

## parameters · peri-cloud/src/archive/tool.rs:18

~~~rust
fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"archive_ref":{"type":"string"},"cursor":{"type":["string","null"]},"budget_tokens":{"type":"integer","minimum":256,"maximum":4096}},"required":["archive_ref"],"additionalProperties":false})
    }
~~~
