<!-- PERI-EDIT-GUIDE:BEGIN -->
> **编辑说明（正式交付时清除）**
> - 功能：工具使用契约。
> - 作用位置：工具目录的 description/schema。
> - 应当写：写何时使用、参数含义、返回值及失败处理；工具名与 schema 字段须和实现一致。
> - 正文中可用 {{宏名}} 标记需求；未实现宏先作为设计标记。保留本段之外的正文作为创作内容。
<!-- PERI-EDIT-GUIDE:END -->

# 工具说明与参数契约

正文为原始工具说明，参数部分为 Rust 生成表达式，字段身份与校验仍由代码保证。

## description · peri-cloud/src/tasks/device_routing.rs:231

## description · peri-cloud/src/tasks/device_routing.rs:311

## parameters · peri-cloud/src/tasks/device_routing.rs:208

~~~rust
fn parameters(native: Value) -> Value {
    let mut properties = native["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    properties.insert("device_id".into(), json!({"type":"string","format":"uuid","description":"Authorized target device ID from list_devices."}));
    properties.insert("workspace".into(), json!({"type":"string","minLength":1,"maxLength":4096,"description":"Target workspace. Omit to pin this device's default workspace for the turn."}));
    let mut required = native["required"].as_array().cloned().unwrap_or_default();
    required.retain(|v| v != "device_id");
    required.push(json!("device_id"));
    let mut schema = native.as_object().cloned().unwrap_or_default();
    schema.insert("type".into(), json!("object"));
    schema.insert("properties".into(), Value::Object(properties));
    schema.insert("required".into(), Value::Array(required));
    schema.insert("additionalProperties".into(), json!(false));
    Value::Object(schema)
}
~~~

## parameters · peri-cloud/src/tasks/device_routing.rs:234

~~~rust
fn parameters(&self) -> Value {
        parameters(self.spec.definition.parameters.clone())
    }
~~~

## parameters · peri-cloud/src/tasks/device_routing.rs:314

~~~rust
fn parameters(&self) -> Value {
        parameters(self.spec.definition.parameters.clone())
    }
~~~

## prompt_declaration · peri-cloud/src/tasks/device_routing.rs:262

~~~rust
fn prompt_declaration(&self) -> Option<String> {
        self.spec.prompt_declaration.clone()
    }
~~~
