//! Display text for the write tool (upstream `renderers/write.ts`). The call shows the file path
//! and a preview of the content; the result only shows text on failure.

use pi_rs_agent::AgentToolResult;
use serde_json::Value;

use super::{string_arg, text_of};

fn trim_trailing_empty_lines<'a>(lines: &'a [&'a str]) -> &'a [&'a str] {
    let mut end = lines.len();
    while end > 0 && lines[end - 1].is_empty() {
        end -= 1;
    }
    &lines[..end]
}

/// `write <path>` followed by at most ten lines of content.
pub fn render_write_call(params: &Value) -> String {
    let path = params
        .get("file_path")
        .filter(|value| value.is_string())
        .or_else(|| params.get("path"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let display = if path.is_empty() { "..." } else { path };
    let mut text = format!("write {display}");

    let Some(content) = string_arg(params.get("content")) else {
        text.push_str("\n\n[invalid content arg - expected string]");
        return text;
    };
    if content.is_empty() {
        return text;
    }

    let normalized = content.replace('\r', "").replace('\t', "   ");
    let split_lines = normalized.split('\n').collect::<Vec<_>>();
    let lines = trim_trailing_empty_lines(&split_lines);
    let shown = lines.len().min(10);
    text.push_str("\n\n");
    text.push_str(&lines[..shown].join("\n"));
    if lines.len() > shown {
        text.push_str(&format!(
            "\n... ({} more lines, {} total, to expand)",
            lines.len() - shown,
            lines.len()
        ));
    }
    text
}

/// Error text only; successful writes render no result output upstream.
pub fn render_write_result(result: &AgentToolResult) -> String {
    if result.is_error != Some(true) {
        return String::new();
    }
    let output = text_of(result);
    if output.is_empty() {
        String::new()
    } else {
        format!("\n{output}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_write_call_with_content_preview() {
        assert_eq!(
            render_write_call(&json!({"path": "f.txt", "content": "hello"})),
            "write f.txt\n\nhello"
        );
        assert_eq!(
            render_write_call(&json!({"path": "f.txt"})),
            "write f.txt\n\n[invalid content arg - expected string]"
        );
    }

    #[test]
    fn truncates_long_content_preview() {
        let content = (1..=12)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rendered = render_write_call(&json!({"path": "f.txt", "content": content}));
        assert!(rendered.contains("line 10"));
        assert!(!rendered.contains("line 11"));
        assert!(rendered.ends_with("\n... (2 more lines, 12 total, to expand)"));
    }

    #[test]
    fn renders_only_error_results() {
        assert_eq!(render_write_result(&AgentToolResult::text("ok")), "");
        let error = AgentToolResult {
            is_error: Some(true),
            ..AgentToolResult::text("failed")
        };
        assert_eq!(render_write_result(&error), "\nfailed");
    }
}
