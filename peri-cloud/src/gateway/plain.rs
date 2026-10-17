//! Conservative IM presentation. Canonical messages and tool data stay intact.
pub(crate) fn plain_chat(input: &str) -> String {
    let mut code = false;
    input
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                code = !code;
                return if code {
                    "代码：".into()
                } else {
                    String::new()
                };
            }
            if code {
                return line.to_owned();
            }
            let heading = trimmed.trim_start_matches('#');
            let line = if heading.len() != trimmed.len() && heading.starts_with(' ') {
                heading.trim_start()
            } else {
                line
            };
            let line = if line.starts_with("**") && line.ends_with("**") && line.len() > 4 {
                &line[2..line.len() - 2]
            } else {
                line
            };
            let mut rest = line;
            let mut output = String::new();
            while let Some(start) = rest.find('[') {
                let Some(middle) = rest[start + 1..].find("](").map(|m| start + 1 + m) else {
                    break;
                };
                let Some(end) = rest[middle + 2..].find(')').map(|e| middle + 2 + e) else {
                    break;
                };
                let address = &rest[middle + 2..end];
                if !(address.starts_with("https://") || address.starts_with("http://")) {
                    break;
                }
                output.push_str(&rest[..start]);
                output.push_str(&rest[start + 1..middle]);
                output.push('（');
                output.push_str(address);
                output.push('）');
                rest = &rest[end + 1..];
            }
            output.push_str(rest);
            output
        })
        .collect::<Vec<_>>()
        .join("\n")
}
#[cfg(test)]
mod tests {
    #[test]
    fn headings_links_and_fences_are_readable_without_altering_code() {
        let source="# 状态\n**已完成**\n[结果](https://example.test/task?id=7)\n```python\n# keep this\nx = 2**3\n```";
        assert_eq!(
            super::plain_chat(source),
            "状态\n已完成\n结果（https://example.test/task?id=7）\n代码：\n# keep this\nx = 2**3\n"
        );
    }
}
