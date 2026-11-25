//! Pure display renderers for the built-in tools, ported from
//! `packages/coding-agent/src/core/tools/renderers/`.
//!
//! Upstream renderers are TUI components colored by a theme. These functions return the same plain
//! text without ANSI codes, which is what a non-interactive display or a test needs. The `call`
//! functions take the raw tool arguments; the `result` functions take an `AgentToolResult`.

pub mod bash;
pub mod edit;
pub mod find;
pub mod grep;
pub mod ls;
pub mod read;
pub mod write;

pub use bash::{
    render_bash_call, render_bash_result, render_powershell_call, render_powershell_result,
};
pub use edit::{render_edit_call, render_edit_result};
pub use find::{render_find_call, render_find_result};
pub use grep::{render_grep_call, render_grep_result};
pub use ls::{render_ls_call, render_ls_result};
pub use read::{render_read_call, render_read_result};
pub use write::{render_write_call, render_write_result};

use pi_rs_agent::AgentToolResult;
use pi_rs_ai::InputContent;
use serde_json::Value;

/// Join the text content blocks the way upstream's `getTextOutput` does (without ANSI or CR).
pub(crate) fn text_of(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            InputContent::Text(text) => Some(text.text.replace('\r', "")),
            InputContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A string argument, or `None` when missing or of the wrong type.
pub(crate) fn string_arg(value: Option<&Value>) -> Option<&str> {
    match value {
        Some(Value::String(text)) => Some(text),
        _ => None,
    }
}

/// Render a number the way JavaScript's `String(number)` does.
pub(crate) fn number_text(value: Option<&Value>) -> Option<String> {
    let number = value?.as_f64()?;
    if number.fract() == 0.0 && number.is_finite() {
        Some(format!("{}", number as i64))
    } else {
        Some(format!("{number}"))
    }
}

/// Parse a number from either a JSON number or a numeric string (models send both).
pub(crate) fn parse_number(value: Option<&Value>) -> Option<f64> {
    let number = match value {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
        _ => None,
    };
    number.filter(|value| value.is_finite())
}

/// Read a `usize` from a details object field.
pub(crate) fn detail_usize(details: Option<&Value>, key: &str) -> Option<usize> {
    details
        .and_then(|value| value.get(key))
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
}

/// Append a warning line (`[a. b]`) when at least one warning exists.
pub(crate) fn append_warning(text: &mut String, warnings: &[String]) {
    if !warnings.is_empty() {
        text.push_str(&format!("\n[{}]", warnings.join(". ")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_handle_numbers_and_strings() {
        assert_eq!(number_text(Some(&Value::from(5))), Some("5".to_string()));
        assert_eq!(
            number_text(Some(&Value::from(0.5))),
            Some("0.5".to_string())
        );
        assert_eq!(parse_number(Some(&Value::from("25"))), Some(25.0));
        assert_eq!(parse_number(Some(&Value::from("Infinity"))), None);
        assert_eq!(string_arg(Some(&Value::from("x"))), Some("x"));
        assert_eq!(string_arg(Some(&Value::Null)), None);
    }

    #[test]
    fn text_of_skips_images() {
        let result = AgentToolResult::text("hello");
        assert_eq!(text_of(&result), "hello");
    }
}
