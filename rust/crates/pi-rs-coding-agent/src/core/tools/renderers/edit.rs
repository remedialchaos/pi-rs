//! Display text for the edit tool (upstream `renderers/edit.ts`). Upstream renders the diff from
//! the tool details on success and the error text on failure.

use pi_rs_agent::AgentToolResult;
use serde_json::Value;

use super::text_of;

/// `edit <path>`.
pub fn render_edit_call(params: &Value) -> String {
    let path = params
        .get("file_path")
        .filter(|value| value.is_string())
        .or_else(|| params.get("path"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let display = if path.is_empty() { "..." } else { path };
    format!("edit {display}")
}

/// The rendered diff on success, or the error text on failure.
pub fn render_edit_result(result: &AgentToolResult) -> String {
    if result.is_error == Some(true) {
        return text_of(result);
    }
    result
        .details
        .as_ref()
        .and_then(|value| value.get("diff"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_edit_call_with_path_fallback() {
        assert_eq!(render_edit_call(&json!({"path": "f.txt"})), "edit f.txt");
        assert_eq!(
            render_edit_call(&json!({"file_path": "g.txt", "path": "f.txt"})),
            "edit g.txt"
        );
        assert_eq!(render_edit_call(&json!({})), "edit ...");
    }

    #[test]
    fn renders_diff_or_error() {
        let success = AgentToolResult {
            details: Some(json!({"diff": "--- f.txt\n+++ f.txt"})),
            ..AgentToolResult::text("Successfully replaced 1 block(s) in f.txt.")
        };
        assert_eq!(render_edit_result(&success), "--- f.txt\n+++ f.txt");

        let failure = AgentToolResult {
            is_error: Some(true),
            ..AgentToolResult::text("Could not edit file: f.txt. Error code: ENOENT.")
        };
        assert_eq!(
            render_edit_result(&failure),
            "Could not edit file: f.txt. Error code: ENOENT."
        );
    }
}
