//! Quiet tool cards (`apps/p2/plugins/quiet-tools/tui.ts`): collapse each non-error tool call
//! to a single summary line unless `toolCalls` is `"full"` or the transcript is expanded (`Ctrl+O`).

use crate::preferences::{TuiSettingField, TuiSettings};
use crate::render_context::line_text;
use crate::theme::Theme;
use crate::tools::render_utils::shorten_path;
use crate::tools::types::ToolArgs;
use pi_rs_tui::truncate_to_width;

pub fn quiet_tools_setting_field() -> TuiSettingField {
    serde_json::json!({
        "plugin": "quiet-tools",
        "name": "toolCalls",
        "label": "Tool calls",
        "description": "quiet: one line per call (Ctrl+O or a click shows it); full: the tools' own cards",
        "values": ["quiet", "full"],
        "fallback": "quiet",
    })
}

pub fn plugin_tool_calls_is_quiet(settings: Option<&TuiSettings>) -> bool {
    let setting = settings
        .and_then(|s| s.plugins.get("quiet-tools"))
        .and_then(|v| v.get("toolCalls"))
        .and_then(|v| v.as_str());
    setting != Some("full")
}

fn first_non_empty_line(value: &str) -> String {
    let lines: Vec<&str> = value
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let head = line_text(lines.first().copied().unwrap_or_default())
        .trim()
        .to_string();
    if lines.len() > 1 {
        format!("{head} …")
    } else {
        head
    }
}

pub fn quiet_tool_summary(name: &str, args: &ToolArgs, cwd: &str) -> String {
    let path_str = |key: &str, alt: Option<&str>| -> String {
        let raw = args
            .get(key)
            .and_then(|v| v.as_str())
            .or_else(|| alt.and_then(|k| args.get(k).and_then(|v| v.as_str())));
        match raw {
            Some(p) if !p.is_empty() => line_text(&shorten_path(p, cwd)),
            _ => String::new(),
        }
    };
    match name {
        "read" => {
            let p = path_str("path", Some("file_path"));
            if let Some(offset) = args.get("offset").and_then(|v| v.as_i64()) {
                format!("{p}:{offset}")
            } else {
                p
            }
        }
        "write" | "edit" => path_str("path", Some("file_path")),
        "ls" => {
            let p = path_str("path", None);
            if p.is_empty() { ".".to_string() } else { p }
        }
        "grep" | "find" => args
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(line_text)
            .unwrap_or_default(),
        "bash" | "powershell" => args
            .get("command")
            .and_then(|v| v.as_str())
            .map(first_non_empty_line)
            .unwrap_or_default(),
        _ => {
            if let Some(calls) = args.get("calls").and_then(|v| v.as_array()) {
                let names: Vec<&str> = calls
                    .iter()
                    .filter_map(|c| c.get("tool").and_then(|t| t.as_str()))
                    .take(3)
                    .collect();
                if !names.is_empty() {
                    let suffix = if calls.len() > 3 { ", …" } else { "" };
                    return format!("{}{suffix}", names.join(", "));
                }
            }
            args.values()
                .filter_map(|v| v.as_str())
                .find(|s| !s.trim().is_empty())
                .map(first_non_empty_line)
                .unwrap_or_default()
        }
    }
}

pub struct QuietToolOptions<'a> {
    pub cwd: &'a str,
    pub is_partial: bool,
    pub duration_ms: Option<u64>,
    pub output_pad: usize,
}

pub fn render_quiet_tool_line(
    name: &str,
    args: &ToolArgs,
    opts: &QuietToolOptions<'_>,
    theme: &Theme,
    width: usize,
) -> Vec<String> {
    let dur = match opts.duration_ms {
        Some(ms) if ms >= 1000 => format!(" {:.1}s", ms as f64 / 1000.0),
        _ => String::new(),
    };
    let status = if opts.is_partial {
        " …".to_string()
    } else {
        format!(" ✓{dur}")
    };
    let summary = quiet_tool_summary(name, args, opts.cwd);
    let detail = if summary.is_empty() {
        status.trim_start().to_string()
    } else {
        format!("{summary}{status}")
    };
    let text = format!(
        "{} {} {}",
        theme.fg("muted", "›"),
        theme.fg("muted", &theme.bold(&line_text(name))),
        theme.fg("dim", &detail)
    );
    let max = width.saturating_sub(opts.output_pad).max(1);
    vec![format!(
        "{}{}",
        " ".repeat(opts.output_pad),
        truncate_to_width(&text, max, true)
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn quiet_tool_line_renders_single_summary_line() {
        let mut args = HashMap::new();
        args.insert("path".into(), serde_json::json!("/repo/src/main.rs"));
        args.insert("offset".into(), serde_json::json!(42));
        let theme = Theme::default();
        let lines = render_quiet_tool_line(
            "read",
            &args,
            &QuietToolOptions {
                cwd: "/repo",
                is_partial: false,
                duration_ms: Some(1500),
                output_pad: 1,
            },
            &theme,
            80,
        );
        assert_eq!(lines.len(), 1);
        let plain = pi_rs_tui::strip_terminal_sequences(&lines[0]);
        assert_eq!(plain, " › read ./src/main.rs:42 ✓ 1.5s");
    }
}
