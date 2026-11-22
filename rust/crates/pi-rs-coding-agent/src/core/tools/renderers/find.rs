//! Display text for the find tool (upstream `renderers/find.ts`).

use pi_rs_agent::AgentToolResult;
use serde_json::Value;

use super::{number_text, string_arg, text_of};
use crate::core::tools::truncate::{DEFAULT_MAX_BYTES, format_size};

/// `find <pattern> in <path>` with optional limit suffix.
pub fn render_find_call(params: &Value) -> String {
    let pattern = match string_arg(params.get("pattern")) {
        Some(pattern) if !pattern.is_empty() => pattern.to_string(),
        Some(_) => "...".to_string(),
        None => "[invalid arg]".to_string(),
    };
    let path = string_arg(params.get("path"))
        .filter(|path| !path.is_empty())
        .unwrap_or(".");
    let mut text = format!("find {pattern} in {path}");
    if let Some(limit) = number_text(params.get("limit")) {
        text.push_str(&format!(" (limit {limit})"));
    }
    text
}

/// The result list plus upstream's `[Truncated: ...]` warning line.
pub fn render_find_result(result: &AgentToolResult) -> String {
    let mut text = text_of(result).trim().to_string();
    let Some(details) = result.details.as_ref() else {
        return text;
    };
    let mut warnings = Vec::new();
    if let Some(limit) = details.get("resultLimitReached").and_then(Value::as_u64) {
        warnings.push(format!("{limit} results limit"));
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
    fn renders_find_call() {
        assert_eq!(
            render_find_call(&json!({"pattern": "*.ts"})),
            "find *.ts in ."
        );
        assert_eq!(
            render_find_call(&json!({"pattern": "*.ts", "path": "src", "limit": 5})),
            "find *.ts in src (limit 5)"
        );
        assert_eq!(render_find_call(&json!({})), "find [invalid arg] in .");
    }

    #[test]
    fn renders_find_result_with_warning() {
        let result = AgentToolResult {
            details: Some(json!({"resultLimitReached": 1000})),
            ..AgentToolResult::text("a.ts\nb.ts")
        };
        assert_eq!(
            render_find_result(&result),
            "a.ts\nb.ts\n[Truncated: 1000 results limit]"
        );
    }
}
