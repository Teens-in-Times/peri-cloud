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
