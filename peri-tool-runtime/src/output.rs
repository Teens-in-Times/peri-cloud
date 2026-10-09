//! Native tool output persistence and UTF-8-safe truncation.

// ── 输出截断落盘（bg shell 执行链共用）───────────────────────────────────────

/// 当输出被截断时，将完整内容写入临时文件。
/// 返回追加到截断信息后的提示字符串。
/// 文件路径：`{temp_dir}/peri-tool-output-{uuid}.txt`
pub fn persist_truncated_output(full_content: &str) -> String {
    let (hint, _) = persist_truncated_output_with_ref(full_content);
    hint
}

/// Persist a full output and return both the display hint and durable path.
/// The caller should carry the path as typed evidence instead of recovering it
/// from rendered text.
pub fn persist_truncated_output_with_ref(full_content: &str) -> (String, Option<String>) {
    let id = uuid::Uuid::new_v4();
    let dir = std::env::temp_dir();
    let file_name = format!("peri-tool-output-{id}.txt");
    let file_path = dir.join(&file_name);

    match std::fs::write(&file_path, full_content) {
        Ok(_) => (
            format!(
                "\n\n[Full output saved to {} — use Read tool to view complete content]",
                file_path.display()
            ),
            Some(file_path.to_string_lossy().into_owned()),
        ),
        Err(e) => (
            format!(
                "\n\n[Failed to save full output to {}: {e}]",
                file_path.display()
            ),
            None,
        ),
    }
}

/// 按字节截断字符串，确保不拆分 UTF-8 字符边界。
///
/// 与 `&s[..max_bytes]` 不同，此函数会从 `max_bytes` 位置向前搜索
/// 最近的字符边界，避免在多字节字符中间截断。
pub fn truncate_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}
