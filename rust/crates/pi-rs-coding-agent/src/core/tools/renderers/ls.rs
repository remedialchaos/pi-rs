//! Display text for the ls tool (upstream `renderers/ls.ts`).

use pi_rs_agent::AgentToolResult;
use serde_json::Value;

use super::{number_text, string_arg, text_of};
use crate::core::tools::truncate::{DEFAULT_MAX_BYTES, format_size};

/// `ls <path>` with optional limit suffix. An omitted or empty path displays as `.`.
pub fn render_ls_call(params: &Value) -> String {
    let path = string_arg(params.get("path"))
        .filter(|path| !path.is_empty())
        .unwrap_or(".");
    let mut text = format!("ls {path}");
    if let Some(limit) = number_text(params.get("limit")) {
        text.push_str(&format!(" (limit {limit})"));
    }
    text
}

/// The listing plus upstream's `[Truncated: ...]` warning line.
pub fn render_ls_result(result: &AgentToolResult) -> String {
    let mut text = text_of(result).trim().to_string();
    let Some(details) = result.details.as_ref() else {
        return text;
    };
    let mut warnings = Vec::new();
    if let Some(limit) = details.get("entryLimitReached").and_then(Value::as_u64) {
        warnings.push(format!("{limit} entries limit"));
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
    fn renders_ls_call() {
        assert_eq!(render_ls_call(&json!({})), "ls .");
        assert_eq!(
            render_ls_call(&json!({"path": "src", "limit": 10})),
            "ls src (limit 10)"
        );
    }

    #[test]
    fn renders_ls_result_with_warning() {
        let result = AgentToolResult {
            details: Some(json!({"entryLimitReached": 500})),
            ..AgentToolResult::text("a.txt\nb.txt")
        };
        assert_eq!(
            render_ls_result(&result),
            "a.txt\nb.txt\n[Truncated: 500 entries limit]"
        );
    }
}
