//! Shared native-tool numeric parameter parsing.

/// 严格解析 JSON 数值参数（工具共享，禁止静默回退）。
///
/// - 缺省（null）：`Ok(None)`，由调用方取文档化的默认值；
/// - 非负整数（含 0）：`Ok(Some(n))`；
/// - 浮点（如 `12.5`、`139.0`）、负数、字符串等：`Err`，不再像 `as_u64()`
///   那样静默吞掉并回退默认值——静默回退会让模型以为参数已生效，
///   实际却读到了错误的位置/数量（Read 工具 offset 事故的根因之一）。
pub fn parse_optional_u64(
    value: &serde_json::Value,
    name: &str,
) -> Result<Option<u64>, Box<dyn std::error::Error + Send + Sync>> {
    if value.is_null() {
        return Ok(None);
    }
    let n = value
        .as_f64()
        .ok_or_else(|| format!("Error: '{name}' must be a non-negative integer, got {value}"))?;
    if n.fract() != 0.0 || n < 0.0 {
        return Err(format!("Error: '{name}' must be a non-negative integer, got {n}").into());
    }
    Ok(Some(n as u64))
}
