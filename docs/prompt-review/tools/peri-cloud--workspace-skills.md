<!-- PERI-EDIT-GUIDE:BEGIN -->
> **编辑说明（正式交付时清除）**
> - 功能：工具使用契约。
> - 作用位置：工具目录的 description/schema。
> - 应当写：写何时使用、参数含义、返回值及失败处理；工具名与 schema 字段须和实现一致。
> - 正文中可用 {{宏名}} 标记需求；未实现宏先作为设计标记。保留本段之外的正文作为创作内容。
<!-- PERI-EDIT-GUIDE:END -->

# 工具说明与参数契约

正文为原始工具说明，参数部分为 Rust 生成表达式，字段身份与校验仍由代码保证。

## description · peri-cloud/src/workspace/skills.rs:103

~~~text
Load a project, account or operator skill by catalog name or scoped path. Use DiscoverSkillsTool first if you do not know the name. Global skill paths are cloud guidance references, not target-device filesystem paths.
~~~

## description · peri-cloud/src/workspace/skills.rs:172

~~~text
List effective authorized skills with stable IDs and source references. Filter by query; include_shadowed optionally exposes explicitly selectable alternatives.
~~~

## parameters · peri-cloud/src/workspace/skills.rs:108

~~~rust
fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "skill_name": {
                    "type": "string",
                    "description": "A stable skill_id, catalog name, or catalog path returned by discovery"
                }
            },
            "required": ["skill_name"]
        })
    }
~~~

## parameters · peri-cloud/src/workspace/skills.rs:176

~~~rust
fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Optional case-insensitive filter over skill names and paths"
                },
                "include_shadowed": {"type":"boolean","default":false}
            }
        })
    }
~~~

## prompt_declaration · peri-cloud/src/workspace/skills.rs:133

~~~rust
fn prompt_declaration(&self) -> Option<String> {
        Some(
            "Load a skill's full SKILL.md from the bound device workspace → `{{name}}` \
             ({{title}}), by catalog name or path."
                .to_string(),
        )
    }
~~~

## prompt_declaration · peri-cloud/src/workspace/skills.rs:197

~~~rust
fn prompt_declaration(&self) -> Option<String> {
        Some(
            "List the session's skills from the bound device workspace → `{{name}}` \
             ({{title}}), optionally filtered by a name/path query."
                .to_string(),
        )
    }
~~~

## prompt_sections · peri-cloud/src/workspace/skills.rs:48

~~~rust
fn prompt_sections(&self) -> Vec<PromptSection> {
        vec![PromptSection::dynamic(
            "13_skills",
            PromptSectionZone::Uncached,
            6,
            section(),
        )]
    }
~~~

## prompt_contribution · peri-cloud/src/workspace/skills.rs:57

~~~rust
fn prompt_contribution(&self) -> Option<String> {
        Some([section(), self.workspace.summary().unwrap_or_default()].join("\n\n"))
    }
~~~
