//! Display text for the shell tools (upstream `renderers/bash.ts`). Bash and PowerShell share the
//! renderer; they differ only in the prompt (`$` versus `PS>`).

use pi_rs_agent::AgentToolResult;
use serde_json::Value;

use super::{append_warning, detail_usize, number_text, string_arg, text_of};
use crate::core::tools::truncate::{DEFAULT_MAX_BYTES, format_size};

/// `prompt <command>` with an optional ` (timeout <n>s)` suffix.
pub fn render_shell_call(params: &Value, prompt: &str) -> String {
    let command = string_arg(params.get("command"));
    let command = match command {
        None => "[invalid arg]",
        Some("") => "...",
        Some(command) => command,
    };
    let mut text = format!("{prompt} {command}");
    if let Some(timeout) = number_text(params.get("timeout"))
        && timeout != "0"
    {
        text.push_str(&format!(" (timeout {timeout}s)"));
    }
    text
}

pub fn render_bash_call(params: &Value) -> String {
    render_shell_call(params, "$")
}

pub fn render_powershell_call(params: &Value) -> String {
    render_shell_call(params, "PS>")
}

/// The output text with the executor's trailing notice replaced by upstream's warning line.
pub fn render_shell_result(result: &AgentToolResult) -> String {
    let mut output = text_of(result).trim().to_string();
    let Some(details) = result.details.as_ref() else {
        return output;
    };
    let truncation = details.get("truncation");
    let truncated = truncation
        .and_then(|value| value.get("truncated"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let full_path = details.get("fullOutputPath").and_then(Value::as_str);
    let full_error = details.get("fullOutputError").and_then(Value::as_str);
    let footer_marker = full_path.or(full_error);

    if truncated
        && output.ends_with(']')
        && let Some(start) = output.rfind("\n\n[")
    {
        let notice = &output[start..];
        if footer_marker.is_none_or(|marker| notice.contains(marker)) {
            output.truncate(start);
        }
    }

    let mut warnings = Vec::new();
    if let Some(path) = full_path {
        warnings.push(format!("Full output: {path}"));
    } else if let Some(err) = full_error {
        warnings.push(format!("Could not save the full output: {err}"));
    }
    if truncated {
        let by_lines = truncation
            .and_then(|value| value.get("truncatedBy"))
            .and_then(Value::as_str)
            == Some("lines");
        let shown = detail_usize(truncation, "outputLines").unwrap_or(0);
        let total = detail_usize(truncation, "totalLines").unwrap_or(0);
        if by_lines {
            warnings.push(format!("Truncated: showing {shown} of {total} lines"));
        } else {
            let max_bytes = detail_usize(truncation, "maxBytes").unwrap_or(DEFAULT_MAX_BYTES);
            warnings.push(format!(
                "Truncated: {shown} lines shown ({} limit)",
                format_size(max_bytes)
            ));
        }
    }
    append_warning(&mut output, &warnings);
    output
}

pub fn render_bash_result(result: &AgentToolResult) -> String {
    render_shell_result(result)
}

pub fn render_powershell_result(result: &AgentToolResult) -> String {
    render_shell_result(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_shell_calls_with_prompts_and_timeout() {
        let params = json!({"command": "echo hi", "timeout": 5});
        assert_eq!(render_bash_call(&params), "$ echo hi (timeout 5s)");
        assert_eq!(render_powershell_call(&params), "PS> echo hi (timeout 5s)");
        assert_eq!(render_bash_call(&json!({})), "$ [invalid arg]");
        assert_eq!(render_bash_call(&json!({"command": ""})), "$ ...");
    }

    #[test]
    fn renders_result_with_truncation_warning() {
        let result = AgentToolResult {
            details: Some(json!({
                "truncation": {
                    "truncated": true,
                    "truncatedBy": "lines",
                    "outputLines": 2000,
                    "totalLines": 3000,
                    "maxBytes": 51200
                },
                "fullOutputPath": "/tmp/full.txt"
            })),
            ..AgentToolResult::text(
                "tail\n\n[Showing lines 1001-3000 of 3000. Full output: /tmp/full.txt]",
            )
        };
        assert_eq!(
            render_bash_result(&result),
            "tail\n[Full output: /tmp/full.txt. Truncated: showing 2000 of 3000 lines]"
        );
    }
}
