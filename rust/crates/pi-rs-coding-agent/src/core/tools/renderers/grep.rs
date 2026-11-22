//! Display text for the grep tool (upstream `renderers/grep.ts`).

use pi_rs_agent::AgentToolResult;
use serde_json::Value;

use super::{number_text, string_arg, text_of};
use crate::core::tools::truncate::{DEFAULT_MAX_BYTES, format_size};

/// `grep /<pattern>/ in <path>` with optional glob and limit suffixes.
pub fn render_grep_call(params: &Value) -> String {
    let pattern = match string_arg(params.get("pattern")) {
        Some(pattern) => pattern.to_string(),
        None => "[invalid arg]".to_string(),
    };
    let path = string_arg(params.get("path"))
        .filter(|path| !path.is_empty())
        .unwrap_or(".");
    let mut text = format!("grep /{pattern}/ in {path}");
    if let Some(glob) = string_arg(params.get("glob"))
        && !glob.is_empty()
    {
        text.push_str(&format!(" ({glob})"));
    }
    if let Some(limit) = number_text(params.get("limit")) {
        text.push_str(&format!(" limit {limit}"));
    }
    text
}

/// The match output plus upstream's `[Truncated: ...]` warning line.
pub fn render_grep_result(result: &AgentToolResult) -> String {
    let mut text = text_of(result).trim().to_string();
    let Some(details) = result.details.as_ref() else {
        return text;
    };
    let mut warnings = Vec::new();
    if let Some(limit) = details.get("matchLimitReached").and_then(Value::as_u64) {
        warnings.push(format!("{limit} matches limit"));
    }
    if details
        .get("truncation")
        .and_then(|value| value.get("truncated"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let max_bytes = details
            .get("truncation")
            .and_then(|value| value.get("maxBytes"))
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_BYTES as u64);
        warnings.push(format!("{} limit", format_size(max_bytes as usize)));
    }
    if details
        .get("linesTruncated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        warnings.push("some lines truncated".to_string());
    }
    if !warnings.is_empty() {
        text.push_str(&format!("\n[Truncated: {}]", warnings.join(", ")));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_grep_call() {
        assert_eq!(
            render_grep_call(&json!({"pattern": "foo"})),
            "grep /foo/ in ."
        );
        assert_eq!(
            render_grep_call(&json!({
                "pattern": "foo",
                "path": "src",
                "glob": "*.ts",
                "limit": 10
            })),
            "grep /foo/ in src (*.ts) limit 10"
        );
        assert_eq!(render_grep_call(&json!({})), "grep /[invalid arg]/ in .");
    }

    #[test]
    fn renders_grep_result_with_warnings() {
        let result = AgentToolResult {
            details: Some(json!({
                "matchLimitReached": 100,
                "linesTruncated": true
            })),
            ..AgentToolResult::text("f.txt:1: needle")
        };
        assert_eq!(
            render_grep_result(&result),
            "f.txt:1: needle\n[Truncated: 100 matches limit, some lines truncated]"
        );
    }
}
