//! Built-in tool card renderers (`bash`, `powershell`, `edit`, `read`, `write`, `ls`, `grep`, `find`).

use crate::components::diff::{RenderDiffOptions, render_diff};
use crate::components::visual_truncate::{Keep, VisualLinePreview};
use crate::render_context::{format_key_text, line_text, safe_text};
use crate::theme::Theme;
use crate::tools::render_utils::{
    format_tool_call_with_args, get_text_output, invalid_arg_text, render_tool_path, str_value,
};
use crate::tools::shell::create_shell_renderers;
use crate::tools::types::{ToolArgs, ToolRenderContext, ToolRenderers, ToolResultView};
use indexmap::IndexMap;
use pi_rs_tui::{Component, Container, Text};

const DEFAULT_PREVIEW_LINES: usize = 10;

fn expand_hint(
    theme: &Theme,
    context: &ToolRenderContext,
    noun: &str,
) -> impl Fn(usize) -> String + Send + Sync + 'static {
    let key = format_key_text(&context.ui.key_text("app.tools.expand"), false);
    let label = if key.is_empty() {
        "Ctrl+O".to_string()
    } else {
        key
    };
    let theme = theme.clone();
    let noun = noun.to_string();
    move |skipped| {
        theme.fg(
            "muted",
            &format!("... ({skipped} more {noun}, {label} to expand)"),
        )
    }
}

fn preview_result(
    result: &ToolResultView,
    expanded: bool,
    theme: &Theme,
    context: &ToolRenderContext,
) -> Box<dyn Component> {
    let mut container = Container::default();
    let output = get_text_output(Some(result));
    let trimmed = output.trim_end();
    if trimmed.is_empty() {
        return Box::new(container);
    }
    let styled = safe_text(trimmed)
        .split('\n')
        .map(|l| theme.fg("toolOutput", l))
        .collect::<Vec<_>>()
        .join("\n");
    if expanded {
        container.add_child(pi_rs_tui::ComponentRef::new(Text::new(styled, 0, 0, None)));
    } else {
        let preview = VisualLinePreview::new(styled, DEFAULT_PREVIEW_LINES, 80, 0)
            .with_keep(Keep::Start)
            .with_hint(expand_hint(theme, context, "lines"));
        container.add_child(pi_rs_tui::ComponentRef::new(preview));
    }
    Box::new(container)
}

fn edit_renderers() -> ToolRenderers {
    ToolRenderers {
        render_shell: Some("default".into()),
        nested_calls: None,
        render_call: Some(std::rc::Rc::new(|args, theme, context| {
            let raw = args
                .get("path")
                .or_else(|| args.get("file_path"))
                .and_then(str_value);
            let target = render_tool_path(raw.as_deref(), theme, &context.cwd, Some("..."));
            let mut c = Container::default();
            c.add_child(pi_rs_tui::ComponentRef::new(Text::new(
                format!("{} {target}", theme.fg("toolTitle", &theme.bold("edit"))),
                0,
                0,
                None,
            )));
            Box::new(c)
        })),
        render_result: Some(std::rc::Rc::new(|result, options, theme, context| {
            let mut c = Container::default();
            let file_path = context
                .args
                .get("path")
                .or_else(|| context.args.get("file_path"))
                .and_then(str_value);
            if let Some(diff_text) = result
                .details
                .as_ref()
                .and_then(|d| d.get("diff"))
                .and_then(|d| d.as_str())
                && !diff_text.trim().is_empty()
            {
                let rendered = render_diff(
                    &safe_text(diff_text),
                    theme,
                    RenderDiffOptions { file_path },
                );
                if options.expanded {
                    c.add_child(pi_rs_tui::ComponentRef::new(Text::new(
                        rendered, 0, 0, None,
                    )));
                } else {
                    let preview = VisualLinePreview::new(rendered, 16, 80, 0)
                        .with_keep(Keep::Start)
                        .with_hint(expand_hint(theme, context, "diff lines"));
                    c.add_child(pi_rs_tui::ComponentRef::new(preview));
                }
                return Box::new(c);
            }
            preview_result(result, options.expanded, theme, context)
        })),
    }
}

fn path_tool_renderers(title: &'static str, show_offset: bool) -> ToolRenderers {
    ToolRenderers {
        render_shell: Some("default".into()),
        nested_calls: None,
        render_call: Some(std::rc::Rc::new(move |args: &ToolArgs, theme, context| {
            let raw = args
                .get("path")
                .or_else(|| args.get("file_path"))
                .and_then(str_value);
            let mut header = format!(
                "{} {}",
                theme.fg("toolTitle", &theme.bold(title)),
                render_tool_path(raw.as_deref(), theme, &context.cwd, Some("."))
            );
            if show_offset && let Some(offset) = args.get("offset").and_then(|v| v.as_i64()) {
                header.push_str(&theme.fg("muted", &format!(":{offset}")));
            }
            let mut c = Container::default();
            c.add_child(pi_rs_tui::ComponentRef::new(Text::new(header, 0, 0, None)));
            Box::new(c)
        })),
        render_result: Some(std::rc::Rc::new(|result, options, theme, context| {
            preview_result(result, options.expanded, theme, context)
        })),
    }
}

fn search_tool_renderers(title: &'static str) -> ToolRenderers {
    ToolRenderers {
        render_shell: Some("default".into()),
        nested_calls: None,
        render_call: Some(std::rc::Rc::new(move |args: &ToolArgs, theme, context| {
            let pattern = args.get("pattern").and_then(str_value);
            let path = args.get("path").and_then(str_value);
            let mut line = theme.fg("toolTitle", &theme.bold(title));
            match pattern {
                Some(p) => {
                    line.push(' ');
                    line.push_str(&theme.fg("toolOutput", &line_text(&p)));
                }
                None => {
                    line.push(' ');
                    line.push_str(&invalid_arg_text(theme));
                }
            }
            if let Some(p) = path {
                line.push_str(&theme.fg(
                    "muted",
                    &format!(
                        " in {}",
                        render_tool_path(Some(&p), theme, &context.cwd, None)
                    ),
                ));
            } else {
                let _ = &format_tool_call_with_args;
            }
            let mut c = Container::default();
            c.add_child(pi_rs_tui::ComponentRef::new(Text::new(line, 0, 0, None)));
            Box::new(c)
        })),
        render_result: Some(std::rc::Rc::new(|result, options, theme, context| {
            preview_result(result, options.expanded, theme, context)
        })),
    }
}

pub fn create_default_tool_renderers() -> IndexMap<String, ToolRenderers> {
    let mut map = IndexMap::new();
    map.insert("bash".into(), create_shell_renderers("$".into()));
    map.insert("powershell".into(), create_shell_renderers("PS>".into()));
    map.insert("edit".into(), edit_renderers());
    map.insert("read".into(), path_tool_renderers("read", true));
    map.insert("write".into(), path_tool_renderers("write", false));
    map.insert("ls".into(), path_tool_renderers("ls", false));
    map.insert("grep".into(), search_tool_renderers("grep"));
    map.insert("find".into(), search_tool_renderers("find"));
    map
}
