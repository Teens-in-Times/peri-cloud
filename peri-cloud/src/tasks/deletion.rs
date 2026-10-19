//! Conservative semantic fallback after the main assistant reviews exact input.
use serde_json::Value;

pub(super) fn possible(name: &str, input: &Value) -> bool {
    match name {
        // Both operations can replace existing contents. Absence was not
        // established by an authenticated read, so replacement remains possible.
        "Write" | "Edit" => true,
        "Bash" | "Shell" => !plain_read_only(input.get("command").and_then(Value::as_str)),
        _ => false,
    }
}
fn plain_read_only(command: Option<&str>) -> bool {
    let Some(command) = command else {
        return false;
    };
    let command = command.trim();
    if command.is_empty()
        || !command.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    ' ' | '\t' | '_' | '-' | '.' | ',' | ':' | '/' | '\\' | '\'' | '"'
                )
        })
    {
        return false;
    }
    // An intentionally small literal-only shell language. No expansion,
    // redirection, pipeline, statement separator, interpreter or nested code.
    let mut single = false;
    let mut double = false;
    for c in command.chars() {
        if c == '\'' && !double {
            single = !single;
        }
        if c == '"' && !single {
            double = !double;
        }
    }
    if single || double {
        return false;
    }
    let mut words = command.split_whitespace();
    let name = words.next().unwrap_or_default().to_ascii_lowercase();
    match name.as_str() {
        "pwd" | "get-location" => words.next().is_none(),
        "echo" | "write-output" | "printf" | "ls" | "dir" | "get-childitem" | "cat"
        | "get-content" => true,
        _ => false,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn literal_output_is_not_deletion_but_nested_code_and_replacement_need_notice() {
        for command in [
            "echo hello",
            "echo rm discard.txt",
            "Get-ChildItem -Name",
            "pwd",
            "cat source.txt",
        ] {
            assert!(!possible("Bash", &json!({"command":command})), "{command}");
        }
        for command in [
            "echo hi > file.txt",
            "echo $(rm file)",
            "echo `touch file`",
            "ls; rm file",
            "python -c 'from pathlib import Path; Path(\"file\").unlink()'",
            "[IO.File]::Delete('file')",
            "sh -c 'rm file'",
            "echo 'unterminated",
        ] {
            assert!(possible("Bash", &json!({"command":command})), "{command}");
        }
        assert!(possible(
            "Write",
            &json!({"file_path":"file","content":"replacement"})
        ));
        assert!(possible("Edit", &json!({})));
        assert!(!possible("Read", &json!({"file_path":"rm"})));
    }
}
