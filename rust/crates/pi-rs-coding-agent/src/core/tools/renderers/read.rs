//! Display text for the read tool (upstream `renderers/read.ts`).

use pi_rs_agent::AgentToolResult;
use serde_json::Value;

use super::{parse_number, text_of};

fn number_text(value: f64) -> String {
    if value.fract() == 0.0 && value.is_finite() {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// Upstream's `formatReadLineRange`: `:start`, `:start-end`, or empty for malformed ranges.
/// Numeric strings are accepted, and a null argument is treated as omitted.
pub fn format_read_line_range(offset: Option<&Value>, limit: Option<&Value>) -> String {
    let offset_number = parse_number(offset);
    let limit_number = parse_number(limit);
    if offset_number.is_none() && limit_number.is_none() {
        return String::new();
    }
    if offset.is_some_and(|v| !v.is_null()) && offset_number.is_none() {
        return String::new();
    }
    if limit.is_some_and(|v| !v.is_null()) && limit_number.is_none() {
        return String::new();
    }

    let start = offset_number.unwrap_or(1.0);
    let Some(count) = limit_number else {
        return format!(":{}", number_text(start));
    };
    let end = start + count - 1.0;
    if end == 0.0 {
        format!(":{}", number_text(start))
    } else {
        format!(":{}-{}", number_text(start), number_text(end))
    }
}

/// `read <path>:<range>`. Upstream falls back to `file_path` when present.
pub fn render_read_call(params: &Value) -> String {
    let path = params
        .get("file_path")
        .filter(|value| value.is_string())
        .or_else(|| params.get("path"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let display = if path.is_empty() { "..." } else { path };
    let range = format_read_line_range(params.get("offset"), params.get("limit"));
    format!("read {display}{range}")
}

/// The model-facing read output, plus an image warning shown only to the user upstream.
pub fn render_read_result(result: &AgentToolResult) -> String {
    let mut text = text_of(result);
    if let Some(model) = result
        .details
        .as_ref()
        .and_then(|value| value.get("imageUnsupportedByModel"))
        .and_then(Value::as_str)
    {
        text.push_str(&format!(
            "\n[{model} does not support images; it saw a placeholder instead]"
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn formats_line_ranges_like_upstream() {
        assert_eq!(
            format_read_line_range(Some(&json!(25)), Some(&json!(13))),
            ":25-37"
        );
        assert_eq!(
            format_read_line_range(Some(&json!("25")), Some(&json!("13"))),
            ":25-37"
        );
        assert_eq!(
            format_read_line_range(Some(&Value::Null), Some(&json!("13"))),
            ":1-13"
        );
        assert_eq!(format_read_line_range(Some(&json!(25)), None), ":25");
        assert_eq!(format_read_line_range(None, None), "");
        assert_eq!(
            format_read_line_range(Some(&json!("abc")), Some(&json!(13))),
            ""
        );
        assert_eq!(
            format_read_line_range(Some(&json!(25)), Some(&json!(""))),
            ""
        );
    }

    #[test]
    fn renders_read_call_and_result() {
        let params = json!({"path": "notes.txt", "offset": 25, "limit": 13});
        assert_eq!(render_read_call(&params), "read notes.txt:25-37");
        assert_eq!(render_read_call(&json!({})), "read ...");

        let result = AgentToolResult::text("hello");
        assert_eq!(render_read_result(&result), "hello");
        let with_warning = AgentToolResult {
            details: Some(json!({"imageUnsupportedByModel": "text-only"})),
            ..AgentToolResult::text("note")
        };
        assert_eq!(
            render_read_result(&with_warning),
            "note\n[text-only does not support images; it saw a placeholder instead]"
        );
    }
}
