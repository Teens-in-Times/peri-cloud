<!-- PERI-EDIT-GUIDE:BEGIN -->
> **编辑说明（正式交付时清除）**
> - 功能：工具使用契约。
> - 作用位置：工具目录的 description/schema。
> - 应当写：写何时使用、参数含义、返回值及失败处理；工具名与 schema 字段须和实现一致。
> - 正文中可用 {{宏名}} 标记需求；未实现宏先作为设计标记。保留本段之外的正文作为创作内容。
<!-- PERI-EDIT-GUIDE:END -->

# 工具说明与参数契约

正文为原始工具说明，参数部分为 Rust 生成表达式，字段身份与校验仍由代码保证。

## description · peri-middlewares/src/mcp/discover_tool.rs:474

~~~text
只读查询 MCP 域（JSON-RPC 风格，method ∈ search/list/detail）：search 搜索 MCP server/tool/resource/skill/agent，max_results 默认 5、最多 20；list 列出指定服务器四域清单（tools/resources/skills/agents）；detail 查看服务器连接状态/协议版本/capabilities/OAuth。会话目录的 search/list 工具条目包含 execution_name、original_name、server、direct 和保留完整 JSON Schema 的 tool；隐藏工具不会出现。direct 工具按 execution_name 直接调用，deferred 工具经ExecuteExtraTool 调用。未装配目录时返回原始诊断快照，不保证名称可执行。本工具不执行调用；资源读取走 mcp_read_resource，技能加载走 SkillTool。
~~~

## parameters · peri-middlewares/src/mcp/discover_tool.rs:485

~~~rust
fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "method": { "type": "string", "enum": ["search"] },
                        "params": {
                            "type": "object",
                            "properties": {
                                "query": { "type": "string" },
                                "max_results": { "type": "integer", "minimum": 0, "maximum": 20, "default": 5 }
                            },
                            "required": ["query"],
                            "additionalProperties": false
                        }
                    },
                    "required": ["method", "params"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "method": { "type": "string", "enum": ["list"] },
                        "params": {
                            "type": "object",
                            "properties": {
                                "server": { "type": "string" },
                                "domain": { "type": "string", "enum": ["tools", "resources", "skills", "agents"] }
                            },
                            "required": ["server"],
                            "additionalProperties": false
                        }
                    },
                    "required": ["method", "params"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "method": { "type": "string", "enum": ["detail"] },
                        "params": {
                            "type": "object",
                            "properties": { "server": { "type": "string" } },
                            "required": ["server"],
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
