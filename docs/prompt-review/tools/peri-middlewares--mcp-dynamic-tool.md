<!-- PERI-EDIT-GUIDE:BEGIN -->
> **编辑说明（正式交付时清除）**
> - 功能：工具使用契约。
> - 作用位置：工具目录的 description/schema。
> - 应当写：写何时使用、参数含义、返回值及失败处理；工具名与 schema 字段须和实现一致。
> - 正文中可用 {{宏名}} 标记需求；未实现宏先作为设计标记。保留本段之外的正文作为创作内容。
<!-- PERI-EDIT-GUIDE:END -->

# 工具说明与参数契约

正文为原始工具说明，参数部分为 Rust 生成表达式，字段身份与校验仍由代码保证。

## description · peri-middlewares/src/mcp/dynamic/tool.rs:78

~~~text
A canonical Dynamic MCP operation bound before approval.
~~~

## description · peri-middlewares/src/mcp/dynamic/tool.rs:119

~~~text
Load, inspect or unload an MCP server for this session. DynamicMCP is deferred-only. Mutating methods require user approval.
~~~

## parameters · peri-middlewares/src/mcp/dynamic/tool.rs:82

~~~rust
fn parameters(&self) -> Value {
        json!({"type": "object"})
    }
~~~

## parameters · peri-middlewares/src/mcp/dynamic/tool.rs:123

~~~rust
fn parameters(&self) -> Value {
        let secret_ref = json!({
            "type": "object",
            "properties": {"secretRef": {"type": "string"}},
            "required": ["secretRef"],
            "additionalProperties": false
        });
        let subscriptions = json!({
            "type": "object",
            "properties": {
                "resources": {"type": "array", "items": {"type": "string"}},
                "toolsListChanged": {"type": "boolean"},
                "promptsListChanged": {"type": "boolean"},
                "resourcesListChanged": {"type": "boolean"}
            }
        });
        let common_config = json!({
            "timeoutMs": {"type": "integer", "minimum": 1},
            "subscriptions": subscriptions
        });
        let mut stdio_properties = common_config.as_object().unwrap().clone();
        stdio_properties.insert("command".to_string(), json!({"type": "string"}));
        stdio_properties.insert(
            "args".to_string(),
            json!({"type": "array", "items": {"type": "string"}}),
        );
        stdio_properties.insert(
            "env".to_string(),
            json!({"type": "object", "additionalProperties": secret_ref}),
        );
        stdio_properties.insert("cwd".to_string(), json!({"type": "string"}));

        let mut http_properties = common_config.as_object().unwrap().clone();
        http_properties.insert("url".to_string(), json!({"type": "string"}));
        http_properties.insert(
            "headers".to_string(),
            json!({
                "type": "object",
                "additionalProperties": {
                    "oneOf": [
                        {"type": "string"},
                        {
                            "type": "object",
                            "properties": {"secretRef": {"type": "string"}},
                            "required": ["secretRef"],
                            "additionalProperties": false
                        }
                    ]
                }
            }),
        );

        let config = json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": stdio_properties,
                    "required": ["command"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": http_properties,
                    "required": ["url"],
                    "additionalProperties": false
                }
            ]
        });

        json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "method": {"type": "string", "enum": ["load"]},
                        "params": {
                            "type": "object",
                            "properties": {
                                "name": {"type": "string"},
                                "config": config
                            },
                            "required": ["name", "config"],
                            "additionalProperties": false
                        }
                    },
                    "required": ["method", "params"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "method": {"type": "string", "enum": ["status"]},
                        "params": {
                            "oneOf": [
                                {
                                    "type": "object",
                                    "properties": {},
                                    "additionalProperties": false
                                },
                                {
                                    "type": "object",
                                    "properties": {"operationId": {"type": "string"}},
                                    "required": ["operationId"],
                                    "additionalProperties": false
                                },
                                {
                                    "type": "object",
                                    "properties": {"name": {"type": "string"}},
                                    "required": ["name"],
                                    "additionalProperties": false
                                }
                            ]
                        }
                    },
                    "required": ["method"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "method": {"type": "string", "enum": ["unload"]},
                        "params": {
                            "type": "object",
                            "properties": {"name": {"type": "string"}},
                            "required": ["name"],
                            "additionalProperties": false
                        }
                    },
                    "required": ["method", "params"],
                    "additionalProperties": false
                }
            ]
        })
    }
~~~
