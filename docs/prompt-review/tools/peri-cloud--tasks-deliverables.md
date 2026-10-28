<!-- PERI-EDIT-GUIDE:BEGIN -->
> **编辑说明（正式交付时清除）**
> - 功能：工具使用契约。
> - 作用位置：工具目录的 description/schema。
> - 应当写：写何时使用、参数含义、返回值及失败处理；工具名与 schema 字段须和实现一致。
> - 正文中可用 {{宏名}} 标记需求；未实现宏先作为设计标记。保留本段之外的正文作为创作内容。
<!-- PERI-EDIT-GUIDE:END -->

# 工具说明与参数契约

正文为原始工具说明，参数部分为 Rust 生成表达式，字段身份与校验仍由代码保证。

## description · peri-cloud/src/tasks/deliverables.rs:404

~~~text
record_artifact
~~~

~~~text
Register a point-in-time artifact from this task's successful native Read/Write/Edit operation seq. Host derives path, device, workspace and receipt version; do not invent a location or version. A later mutation invalidates the old observation; re-read after final changes. Registration is not acceptance.
~~~

~~~text
record_verification
~~~

~~~text
Record a native Read/Bash/Grep/Glob operation as validation evidence, linking artifact IDs. Host derives pass/failure from typed execution facts; description and semantic coverage are your claims. No mutation may intervene between validation and a later artifact observation. Use current requirements.
~~~

~~~text
review_task
~~~

~~~text
Main assistant reviews a completed execution against latest requirements. Accepted requires current artifact evidence, successful verification IDs covering all artifacts, applied parent instructions and no unverified items. Explain semantic coverage from actual evidence; model self-report alone is insufficient. Rejected records why; use message/control tools for follow-up work.
~~~

~~~text
Inspect bounded task artifacts, validation evidence and acceptance history. accepted is computed against current instructions and receipt versions, separately from loop completion. Evidence is point-in-time, not continuous filesystem monitoring.
~~~

## parameters · peri-cloud/src/tasks/deliverables.rs:412

~~~rust
fn parameters(&self) -> Value {
        let mut props = json!({"task_id":{"type":"string","format":"uuid"}});
        let mut required = vec!["task_id"];
        if self.name != "inspect_deliverables" {
            props["expected_revision"] = json!({"type":"integer","minimum":0});
            required.push("expected_revision");
        }
        if matches!(self.name, "record_artifact" | "record_verification") {
            props["operation"] = json!({"type":"integer","minimum":1});
            props["label"] = json!({"type":"string","maxLength":1000});
            required.extend(["operation", "label"]);
        }
        if self.name == "record_verification" {
            props["artifacts"] =
                json!({"type":"array","maxItems":16,"items":{"type":"string","format":"uuid"}});
        }
        if self.name == "review_task" {
            props["outcome"] = json!({"type":"string","enum":["accepted","rejected"]});
            props["reason"] = json!({"type":"string","maxLength":4000});
            props["verifications"] =
                json!({"type":"array","maxItems":32,"items":{"type":"string","format":"uuid"}});
            props["unverified"] =
                json!({"type":"array","maxItems":16,"items":{"type":"string","maxLength":1000}});
            required.extend(["outcome", "reason"]);
        }
        json!({"type":"object","properties":props,"required":required,"additionalProperties":false})
    }
~~~
