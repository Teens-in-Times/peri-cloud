<!-- PERI-EDIT-GUIDE:BEGIN -->
> **编辑说明（正式交付时清除）**
> - 功能：工具使用契约。
> - 作用位置：工具目录的 description/schema。
> - 应当写：写何时使用、参数含义、返回值及失败处理；工具名与 schema 字段须和实现一致。
> - 正文中可用 {{宏名}} 标记需求；未实现宏先作为设计标记。保留本段之外的正文作为创作内容。
<!-- PERI-EDIT-GUIDE:END -->

# 工具说明与参数契约

正文为原始工具说明，参数部分为 Rust 生成表达式，字段身份与校验仍由代码保证。

## description · peri-cloud/src/scheduler/tools.rs:89

~~~text
schedule_create
~~~

~~~text
Create a persistent scheduled Agent task ONLY when the user asks for future/recurring work. Conversation mode follows this authorized chat across device changes and needs no executor; task mode freezes the current device session and workspace. Use once with explicit RFC3339 offset, or five-field cron with IANA timezone. List first for current UTC time; clarify an unknown user timezone. Normal tool approvals and quotas still apply at execution. Never promise push on an inbox-only entry.
~~~

~~~text
schedule_list
~~~

~~~text
List this account's schedules and current UTC time. Includes revision, next fire, frozen target and notification capability.
~~~

~~~text
schedule_update
~~~

~~~text
schedule_remove
~~~

~~~text
Pause, resume, edit title/prompt/timing, or delete this account's schedule using its latest revision. For edit provide all edit fields; its authorized target cannot change. Does not cancel already claimed/running work. Deleted schedules cannot be resumed.
~~~

~~~text
Read the latest 100 firing receipts for this account's schedule. Skipped and unknown outcomes are never automatically replayed.
~~~

## parameters · peri-cloud/src/scheduler/tools.rs:97

~~~rust
fn parameters(&self) -> Value {
        match self.name {
            "schedule_create" => {
                json!({"type":"object","properties":{"mode":{"enum":["conversation","task"],"description":"conversation (default): personal check-ins/greetings without executor tools; task: actual device work"},"notify":{"enum":["always","when_useful"],"description":"always for an explicitly requested greeting; when_useful lets the Agent stay silent"},"title":{"type":"string","maxLength":128},"prompt":{"type":"string","description":"Future task instruction, without secrets or management slash commands"},"timing":{"oneOf":[{"type":"object","properties":{"kind":{"const":"once"},"at":{"type":"string","description":"RFC3339 with explicit timezone offset"}},"required":["kind","at"],"additionalProperties":false},{"type":"object","properties":{"kind":{"const":"cron"},"expression":{"type":"string"},"timezone":{"type":"string","description":"IANA timezone, e.g. Asia/Shanghai"}},"required":["kind","expression","timezone"],"additionalProperties":false},{"type":"object","properties":{"kind":{"const":"after"},"seconds":{"type":"integer","minimum":1,"maximum":4294967295u64}},"required":["kind","seconds"],"additionalProperties":false},{"type":"object","properties":{"kind":{"const":"interval"},"anchor":{"type":"integer"},"every_seconds":{"type":"integer","minimum":1,"maximum":4294967295u64}},"required":["kind","anchor","every_seconds"],"additionalProperties":false}]}},"required":["title","prompt","timing"],"additionalProperties":false})
            }
            "schedule_update" | "schedule_remove" => {
                let mut schema = json!({"type":"object","properties":{"id":{"type":"string"},"revision":{"type":"integer"},"action":{"enum":["pause","resume","delete","edit"]},"edit":{"type":"object","properties":{"title":{"type":"string","maxLength":128},"prompt":{"type":"string","maxLength":8192},"timing":{"type":"object"},"notify":{"enum":["always","when_useful"]},"enabled":{"type":"boolean"}},"required":["title","prompt","timing","notify","enabled"],"additionalProperties":false}},"required":["id","revision","action"],"additionalProperties":false});
                if self.name == "schedule_remove" {
                    schema["properties"]["action"] = json!({"const":"delete"});
                }
                schema
            }
            "schedule_runs" => {
                json!({"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false})
            }
            _ => json!({"type":"object","properties":{},"additionalProperties":false}),
        }
    }
~~~
