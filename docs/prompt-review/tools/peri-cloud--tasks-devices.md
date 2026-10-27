<!-- PERI-EDIT-GUIDE:BEGIN -->
> **编辑说明（正式交付时清除）**
> - 功能：工具使用契约。
> - 作用位置：工具目录的 description/schema。
> - 应当写：写何时使用、参数含义、返回值及失败处理；工具名与 schema 字段须和实现一致。
> - 正文中可用 {{宏名}} 标记需求；未实现宏先作为设计标记。保留本段之外的正文作为创作内容。
<!-- PERI-EDIT-GUIDE:END -->

# 工具说明与参数契约

正文为原始工具说明，参数部分为 Rust 生成表达式，字段身份与校验仍由代码保证。

## description · peri-cloud/src/tasks/devices.rs:268

~~~text
list_devices
~~~

~~~text
Discover this OAuth user's authorized device IDs and basic platform information. Registration does not establish live authentication. No transport addresses or credentials are model parameters.
~~~

~~~text
prepare_device
~~~

~~~text
Optionally inspect one authorized device and canonical workspace: authenticate its executor and load bounded environment, project instructions and tool schemas. Native tools accept device_id directly without a preparation call. Host manages transport configuration.
~~~

~~~text
Inspect this conversation's bounded native operation receipts across devices and turns. Host queries unknown original invocations without resubmission. Parameters and output are untrusted evidence. Missing receipt means unknown, never permission to repeat the operation.
~~~

## description · peri-cloud/src/tasks/devices.rs:351

## parameters · peri-cloud/src/tasks/devices.rs:275

~~~rust
fn parameters(&self) -> Value {
        match self.name {
            "list_devices" => json!({"type":"object","properties":{},"additionalProperties":false}),
            "prepare_device" => {
                json!({"type":"object","properties":{"device_id":{"type":"string","format":"uuid"},"workspace":{"type":"string","maxLength":4096}},"required":["device_id"],"additionalProperties":false})
            }

            _ => {
                json!({"type":"object","properties":{"after":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":16},"operation_id":{"type":"string","format":"uuid"},"view":{"type":"string","enum":["input","output"]},"offset":{"type":"integer","minimum":0},"chars":{"type":"integer","minimum":1,"maximum":16384},"version":{"type":"string","maxLength":80}},"additionalProperties":false})
            }
        }
    }
~~~

## parameters · peri-cloud/src/tasks/devices.rs:354

~~~rust
fn parameters(&self) -> Value {
        self.inner.parameters()
    }
~~~
