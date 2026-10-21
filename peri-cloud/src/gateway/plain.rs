//! IM presentation only: canonical messages and authenticated tool details stay intact.
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

pub(crate) fn plain_chat(input: &str) -> String {
    let mut out = String::new();
    let mut links = Vec::new();
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut cell = 0;
    let mut structural_tail = false;
    for event in Parser::new_ext(
        input,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS,
    ) {
        match &event {
            Event::Text(_)
            | Event::Code(_)
            | Event::Html(_)
            | Event::InlineHtml(_)
            | Event::End(TagEnd::CodeBlock) => structural_tail = false,
            Event::End(TagEnd::Paragraph | TagEnd::Heading(_)) => {
                structural_tail = !out.ends_with('\n')
            }
            _ => {}
        }
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph | Tag::Heading { .. } | Tag::BlockQuote(_) => newline(&mut out),
                Tag::CodeBlock(_) => {
                    newline(&mut out);
                    out.push_str("代码：\n");
                }
                Tag::List(start) => {
                    newline(&mut out);
                    lists.push(start);
                }
                Tag::Item => {
                    newline(&mut out);
                    out.push_str(&"  ".repeat(lists.len().saturating_sub(1)));
                    if let Some(Some(n)) = lists.last_mut() {
                        out.push_str(&format!("{n}. "));
                        *n += 1;
                    } else {
                        out.push_str("• ");
                    }
                }
                Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                    links.push((out.len(), dest_url.to_string()))
                }
                Tag::TableHead | Tag::TableRow => {
                    newline(&mut out);
                    cell = 0;
                }
                Tag::TableCell => {
                    if cell > 0 {
                        out.push_str(" ｜ ");
                    }
                    cell += 1;
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph
                | TagEnd::Heading(_)
                | TagEnd::BlockQuote(_)
                | TagEnd::CodeBlock
                | TagEnd::TableHead
                | TagEnd::TableRow
                | TagEnd::Table
                | TagEnd::Item => newline(&mut out),
                TagEnd::List(_) => {
                    lists.pop();
                    newline(&mut out);
                }
                TagEnd::Link | TagEnd::Image => {
                    if let Some((start, address)) = links.pop() {
                        if (address.starts_with("https://") || address.starts_with("http://"))
                            && out[start..] != address
                        {
                            out.push('（');
                            out.push_str(&address);
                            out.push('）');
                        }
                    }
                }
                _ => {}
            },
            Event::Text(text) | Event::Code(text) | Event::Html(text) | Event::InlineHtml(text) => {
                out.push_str(&text)
            }
            Event::SoftBreak | Event::HardBreak | Event::Rule => newline(&mut out),
            Event::TaskListMarker(done) => out.push_str(if done { "[✓] " } else { "[ ] " }),
            Event::FootnoteReference(label) => {
                out.push('[');
                out.push_str(&label);
                out.push(']');
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => out.push_str(&text),
        }
    }
    // Remove only the renderer's terminal newline, never code payload bytes.
    if structural_tail && !input.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

fn newline(out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn markdown_degrades_without_changing_code_or_losing_urls_and_table_cells() {
        let source = "# 状态\n**已完成**\n[结果](https://example.test/task?id=7)\n```python\n# keep this\nx = 2**3\n```";
        assert_eq!(
            super::plain_chat(source),
            "状态\n已完成\n结果（https://example.test/task?id=7）\n代码：\n# keep this\nx = 2**3\n"
        );
        let table = super::plain_chat("|项目|结果|\n|---|---|\n|**写入**|成功|\n\n- [x] 完成");
        assert!(table.contains("项目 ｜ 结果\n写入 ｜ 成功"));
        assert!(table.contains("• [✓] 完成"));
        assert_eq!(super::plain_chat("`a*b` and *text*"), "a*b and text");
    }
}
