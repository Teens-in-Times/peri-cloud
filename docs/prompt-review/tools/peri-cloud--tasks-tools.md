<!-- PERI-EDIT-GUIDE:BEGIN -->
> **编辑说明（正式交付时清除）**
> - 功能：工具使用契约。
> - 作用位置：工具目录的 description/schema。
> - 应当写：写何时使用、参数含义、返回值及失败处理；工具名与 schema 字段须和实现一致。
> - 正文中可用 {{宏名}} 标记需求；未实现宏先作为设计标记。保留本段之外的正文作为创作内容。
<!-- PERI-EDIT-GUIDE:END -->

# 工具说明与参数契约

正文为原始工具说明，参数部分为 Rust 生成表达式，字段身份与校验仍由代码保证。

## description · peri-cloud/src/tasks/tools.rs:130

~~~text
delegate_task
~~~

~~~text
Persist an independent cloud execution task and return immediately. Supply goal, constraints and acceptance; optional device/workspace must belong to this user. Native tools remain available for small operations. Workers use their own context and the original tool approval policy.
~~~

~~~text
list_execution_sessions
~~~

~~~text
List authorized execution conversations for explicit reuse. Reuse preserves work context across independent tasks, within the same device, canonical workspace and privacy scope. Supply the execution_session plus matching device/workspace to delegate_task.
~~~

~~~text
list_tasks
~~~

~~~text
List paginated bounded task history for this logical conversation, newest first; pass next_cursor as after. Runtime state and worker-authored notes are separate. Never poll continuously while the user wants to chat.
~~~

~~~text
inspect_task
~~~

~~~text
Read status, full task contract, approval/operation receipts, windows, bounded trace, conversation or messages. related_tasks pages prior tasks in the same explicitly reused execution session; these are historical evidence and do not grant control over other tasks. Trace supports kind/query filters. Use detail with source/id/offset/chars/version for complete evidence in bounded versioned chunks; preserve the returned version across pages. For archived context, list windows then select window_id in conversation. Cropped previews and agent claims are not complete evidence; private reasoning is excluded.
~~~

~~~text
message_task
~~~

~~~text
Send a supplement or question to this conversation's task at its current revision. Submitted, consumed and applied are different receipts. A message cannot authorize additional permissions.
~~~

~~~text
control_task
~~~

~~~text
Control a task at its current revision: pause at a safe boundary, resume a durable wait/paused task, rotate its context window, or cancel. Unknown execution is queried before any continuation; cancellation does not prove that an effect stopped.
~~~

~~~text
decide_approval
~~~

~~~text
Record a decision on a complete pending worker approval presented in this main turn. Uses the current task requirements and exact calls from the host frame; host permission and notification checks still govern execution.
~~~

~~~text
wait_task
~~~

~~~text
Durably wait for an external condition. Supply reason and optional resume_after_seconds (up to one week). Host stops at a safe boundary and releases the loop slot; main assistant can resume explicitly.
~~~

~~~text
update_task
~~~

~~~text
Update this execution task's work plan, discoveries and blockers, and explicitly acknowledge applied parent messages. These are worker-authored claims; they do not overwrite host lifecycle state. Trace is collected automatically.
~~~

~~~text
Send a message or question to the main assistant through its durable conversation queue. For a question requiring a response, set wait_for_reply=true; the host durably suspends this run and resumes on the parent response while the main assistant remains free to chat. Do not send ordinary user messages directly.
~~~

## parameters · peri-cloud/src/tasks/tools.rs:144

~~~rust
fn parameters(&self) -> Value {
        let id = json!({"type":"string","format":"uuid"});
        let rev = json!({"type":"integer","minimum":0});
        let (properties, required) = match self.name {
            "delegate_task" => (
                json!({"goal":{"type":"string","maxLength":16384},"constraints":{"type":"string","maxLength":16384},"acceptance":{"type":"string","maxLength":8192},"device_id":id,"workspace":{"type":"string"},"execution_session":id}),
                vec!["goal"],
            ),
            "list_execution_sessions" => (json!({}), vec![]),
            "list_tasks" => (
                json!({"after":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":32}}),
                vec![],
            ),
            "inspect_task" => (
                json!({"task_id":id,"view":{"type":"string","enum":["status","trace","conversation","messages","operations","windows","contract","approvals","detail","related_tasks"]},"window_id":id,"after":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":32},"kind":{"type":"string","maxLength":80},"query":{"type":"string","maxLength":200},"detail":{"type":"object","properties":{"source":{"type":"string","enum":["operation_input","operation_output","approval_input","conversation"]},"id":{"type":"string","maxLength":256},"offset":{"type":"integer","minimum":0},"chars":{"type":"integer","minimum":1,"maximum":16384},"version":{"type":"string","maxLength":80}},"required":["source","id"],"additionalProperties":false}}),
                vec!["task_id"],
            ),
            "message_task" => (
                json!({"task_id":id,"expected_revision":rev,"body":{"type":"string","maxLength":16384}}),
                vec!["task_id", "expected_revision", "body"],
            ),
            "decide_approval" => (
                json!({"request_id":id,"approve":{"type":"boolean"},"deletion_possible":{"type":"boolean"},"reason":{"type":"string","maxLength":2000}}),
                vec!["request_id", "approve", "reason"],
            ),
            "control_task" => (
                json!({"task_id":id,"expected_revision":rev,"action":{"type":"string","enum":["cancel","pause","resume","rotate"]}}),
                vec!["task_id", "expected_revision", "action"],
            ),
            "update_task" => (
                json!({"task_id":id,"expected_revision":rev,"note":{"type":"string","maxLength":4000},"work_plan":{"type":"array","maxItems":16,"items":{"type":"object","properties":{"name":{"type":"string"},"state":{"type":"string","enum":["pending","in_progress","done","blocked"]}},"required":["name","state"],"additionalProperties":false}},"applied_messages":{"type":"array","items":{"type":"integer"},"maxItems":16}}),
                vec!["task_id", "expected_revision", "note"],
            ),
            "wait_task" => (
                json!({"reason":{"type":"string","maxLength":4000},"resume_after_seconds":{"type":"integer","minimum":0,"maximum":604800}}),
                vec!["reason"],
            ),
            _ => (
                json!({"body":{"type":"string","maxLength":16384},"wait_for_reply":{"type":"boolean"}}),
                vec!["body"],
            ),
        };
        json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
    }
~~~
