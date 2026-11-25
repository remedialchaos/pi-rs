use crate::theme::Theme;
use crate::tools::types::{ToolResultContent, ToolResultView};
use pi_rs_tui::image_fallback;

pub fn shorten_path(path: &str, cwd: &str) -> String {
    if let Some(rest) = path.strip_prefix(cwd) {
        if rest.starts_with('/') {
            return format!(".{}", rest);
        }
        return format!("./{}", rest);
    }
    path.to_string()
}

pub fn str_value(value: &serde_json::Value) -> Option<String> {
    value.as_str().map(|s| s.to_string())
}

pub fn num_value(value: &serde_json::Value) -> Option<f64> {
    value.as_f64()
}

pub fn replace_tabs(text: &str) -> String {
    text.replace('\t', "   ")
}

pub fn normalize_display_text(text: &str) -> String {
    text.replace('\r', "")
}

pub fn get_text_output(result: Option<&ToolResultView>) -> String {
    let Some(result) = result else {
        return String::new();
    };
    let mut output = String::new();
    for content in &result.content {
        match content {
            ToolResultContent::Text(text) => output.push_str(&text.text),
            ToolResultContent::Image(img) => {
                output.push_str(&image_fallback(&img.mime_type, None, None))
            }
        }
    }
    output
}

const COLLAPSED_ARGS_CHARS: usize = 100;

pub fn format_tool_call_with_args(
    title: &str,
    args: &serde_json::Value,
    theme: &Theme,
    expanded: bool,
) -> String {
    let args_str = if expanded {
        serde_json::to_string_pretty(args).unwrap_or_default()
    } else {
        serde_json::to_string(args).unwrap_or_default()
    };
    if args_str.len() > COLLAPSED_ARGS_CHARS && !expanded {
        format!(
            "{} {}...",
            theme.bold(title),
            args_str
                .chars()
                .take(COLLAPSED_ARGS_CHARS)
                .collect::<String>()
        )
    } else {
        format!("{} {}", theme.bold(title), args_str)
    }
}

pub fn invalid_arg_text(theme: &Theme) -> String {
    theme.fg("error", "[invalid arg]")
}

pub fn render_tool_path(
    raw_path: Option<&str>,
    theme: &Theme,
    cwd: &str,
    empty_fallback: Option<&str>,
) -> String {
    match raw_path {
        Some(path) if !path.is_empty() => {
            let shortened = shorten_path(path, cwd);
            theme.fg("toolOutput", &shortened)
        }
        _ => empty_fallback
            .map(|s| theme.fg("toolOutput", s))
            .unwrap_or_else(|| theme.fg("toolOutput", "...")),
    }
}

pub fn more_lines_hint(
    ui: &dyn crate::render_context::RenderContext,
    _theme: &Theme,
    remaining: usize,
    noun: &str,
) -> String {
    format!(
        "{} (to expand)",
        crate::render_context::key_hint(ui, "app.tools.expand", &format!("{} {}", remaining, noun))
    )
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{:.1} {}", size, UNITS[unit])
    }
}

pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
pub const DEFAULT_MAX_LINES: usize = 2000;

pub fn trim_trailing_empty_lines(lines: &mut Vec<String>) {
    while lines.last().map(|s| s.trim().is_empty()).unwrap_or(false) {
        lines.pop();
    }
}
