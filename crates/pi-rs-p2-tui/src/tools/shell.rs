use crate::components::visual_truncate::{Keep, VisualLinePreview};
use crate::render_context::{format_key_text, safe_text};
use crate::theme::Theme;
use crate::tools::render_utils::{get_text_output, invalid_arg_text, num_value, str_value};
use crate::tools::types::{
    ToolArgs, ToolRenderContext, ToolRenderResultOptions, ToolRenderers, ToolResultView,
};
use pi_rs_tui::{Component, Container, Spacer, Text};

pub const BASH_PREVIEW_LINES: usize = 5;

pub fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn format_duration(ms: u64) -> String {
    let seconds = ms as f64 / 1000.0;
    if seconds < 60.0 {
        return format!("{:.1}s", seconds);
    }
    let total_seconds = seconds as u64;
    let minutes = total_seconds / 60;
    let remainder = total_seconds % 60;
    if minutes < 60 {
        return format!("{}m {}s", minutes, remainder);
    }
    format!("{}h {}m {}s", minutes / 60, minutes % 60, remainder)
}

pub fn format_shell_call(args: &ToolArgs, prompt: &str, theme: &Theme) -> String {
    let raw_command = args.get("command").and_then(str_value);
    let timeout = args.get("timeout").and_then(num_value);
    let timeout_suffix = timeout
        .map(|t| theme.fg("muted", &format!(" (timeout {}s)", t as u64)))
        .unwrap_or_default();
    let command_display = match raw_command {
        None => invalid_arg_text(theme),
        Some(cmd) if cmd.is_empty() => theme.fg("toolOutput", "..."),
        Some(cmd) => safe_text(&cmd),
    };
    format!(
        "{}{}",
        theme.fg(
            "toolTitle",
            &theme.bold(&format!("{} {}", prompt, command_display))
        ),
        timeout_suffix
    )
}

struct LiveElapsedText {
    started_at_ms: u64,
    theme: Theme,
}

impl Component for LiveElapsedText {
    fn render(&self, width: usize) -> Vec<String> {
        let elapsed = now_epoch_ms().saturating_sub(self.started_at_ms);
        let text = Text::new(
            self.theme
                .fg("muted", &format!("Elapsed {}", format_duration(elapsed))),
            0,
            0,
            None,
        );
        text.render(width)
    }
}

pub fn rebuild_shell_result(
    component: &mut Container,
    result: &ToolResultView,
    options: &ToolRenderResultOptions,
    theme: &Theme,
    context: &ToolRenderContext,
    started_at: Option<u64>,
    ended_at: Option<u64>,
) {
    component.clear();
    let raw_output = get_text_output(Some(result));
    let output = raw_output.trim();

    if !output.is_empty() {
        let styled = safe_text(output)
            .split('\n')
            .map(|line| theme.fg("toolOutput", line))
            .collect::<Vec<_>>()
            .join("\n");
        if options.expanded {
            component.add_child(pi_rs_tui::ComponentRef::new(Text::new(styled, 0, 0, None)));
        } else {
            let expand_key = format_key_text(&context.ui.key_text("app.tools.expand"), false);
            let expand_label = if expand_key.is_empty() {
                "Ctrl+O".to_string()
            } else {
                expand_key
            };
            let muted_theme = theme.clone();
            let preview = VisualLinePreview::new(styled, BASH_PREVIEW_LINES, 80, 0)
                .with_keep(Keep::End)
                .with_hint(move |skipped| {
                    muted_theme.fg(
                        "muted",
                        &format!("... ({skipped} earlier lines, {expand_label} to expand)"),
                    )
                });
            component.add_child(pi_rs_tui::ComponentRef::new(preview));
        }
    }

    if let Some(details) = result.details.as_ref().and_then(|d| d.as_object())
        && details
            .get("truncation")
            .and_then(|t| t.get("truncated"))
            .and_then(|v| v.as_bool())
            == Some(true)
        && let Some(full_path) = details.get("fullOutputPath").and_then(|v| v.as_str())
    {
        component.add_child(pi_rs_tui::ComponentRef::new(Text::new(
            theme.fg(
                "warning",
                &format!("\nOutput truncated. Full output: {}", safe_text(full_path)),
            ),
            0,
            0,
            None,
        )));
    }

    let state_started =
        started_at.or_else(|| context.state.get("startedAt").and_then(|v| v.as_u64()));
    let state_ended = ended_at.or_else(|| context.state.get("endedAt").and_then(|v| v.as_u64()));

    if options.is_partial {
        if let Some(started) = state_started {
            component.add_child(pi_rs_tui::ComponentRef::new(Spacer::new(1)));
            component.add_child(pi_rs_tui::ComponentRef::new(LiveElapsedText {
                started_at_ms: started,
                theme: theme.clone(),
            }));
        }
    } else if let Some(duration) = context
        .duration_ms
        .or_else(|| Some(state_ended?.saturating_sub(state_started?)))
    {
        component.add_child(pi_rs_tui::ComponentRef::new(Spacer::new(1)));
        component.add_child(pi_rs_tui::ComponentRef::new(Text::new(
            theme.fg("muted", &format!("Took {}", format_duration(duration))),
            0,
            0,
            None,
        )));
    }
}

pub fn create_shell_renderers(prompt: String) -> ToolRenderers {
    ToolRenderers {
        render_shell: Some("default".to_string()),
        nested_calls: None,
        render_call: Some(std::rc::Rc::new(move |args, theme, _context| {
            let text = format_shell_call(args, &prompt, theme);
            let mut container = Container::default();
            container.add_child(pi_rs_tui::ComponentRef::new(Text::new(text, 0, 0, None)));
            Box::new(container)
        })),
        render_result: Some(std::rc::Rc::new(|result, options, theme, context| {
            let mut container = Container::default();
            rebuild_shell_result(&mut container, result, options, theme, context, None, None);
            Box::new(container)
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_context::RenderContext;
    use crate::theme::ThemeRef;
    use crate::tools::types::{ToolResultContent, ToolView};
    use std::collections::HashMap;

    struct DummyUi(ThemeRef);
    impl RenderContext for DummyUi {
        fn theme(&self) -> &ThemeRef {
            &self.0
        }
        fn key_text(&self, id: &str) -> String {
            if id == "app.tools.expand" {
                "ctrl+o".to_string()
            } else {
                String::new()
            }
        }
        fn request_render(&self) {}
    }

    fn test_ctx(expanded: bool, is_partial: bool, duration_ms: Option<u64>) -> ToolRenderContext {
        let mut state = HashMap::new();
        state.insert("startedAt".to_string(), serde_json::json!(1_000));
        if !is_partial {
            state.insert("endedAt".to_string(), serde_json::json!(3_200));
        }
        ToolRenderContext {
            args: HashMap::new(),
            tool_call_id: "call_1".into(),
            invalidate: Box::new(|| {}),
            last_component: None,
            state,
            cwd: "/tmp".into(),
            execution_started: true,
            args_complete: !is_partial,
            is_partial,
            expanded,
            show_images: false,
            is_error: false,
            duration_ms,
            output_pad: 1,
            calls: None,
            ui: Box::new(DummyUi(ThemeRef::new(Theme::default()))),
        }
    }

    #[test]
    fn empty_command_shows_ellipsis_and_missing_command_shows_invalid_arg() {
        let theme = Theme::default();
        let mut empty_args = HashMap::new();
        empty_args.insert("command".into(), serde_json::json!(""));
        let rendered_empty =
            pi_rs_tui::strip_terminal_sequences(&format_shell_call(&empty_args, "$", &theme));
        assert_eq!(rendered_empty, "$ ...");

        let missing_args = HashMap::new();
        let rendered_missing =
            pi_rs_tui::strip_terminal_sequences(&format_shell_call(&missing_args, "$", &theme));
        assert!(
            rendered_missing.contains("[invalid arg]"),
            "{rendered_missing}"
        );
    }

    #[test]
    fn shell_result_truncates_with_expand_hint_when_collapsed_and_shows_all_when_expanded() {
        let theme = Theme::default();
        let lines = (1..=12)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = ToolResultView {
            content: vec![ToolResultContent::Text(lines.into())],
            details: None,
        };
        let mut container = Container::default();

        let collapsed_ctx = test_ctx(false, false, Some(2200));
        rebuild_shell_result(
            &mut container,
            &result,
            &ToolRenderResultOptions {
                expanded: false,
                is_partial: false,
            },
            &theme,
            &collapsed_ctx,
            None,
            None,
        );
        let collapsed_out = container.render(80);
        let collapsed_plain = pi_rs_tui::strip_terminal_sequences(&collapsed_out.join("\n"));
        assert!(!collapsed_plain.contains("line 1\n"), "{collapsed_plain}");
        assert!(collapsed_plain.contains("line 12"), "{collapsed_plain}");
        assert!(
            collapsed_plain.contains("... (7 earlier lines, ctrl+o to expand)"),
            "{collapsed_plain}"
        );
        assert!(collapsed_plain.contains("Took 2.2s"), "{collapsed_plain}");

        let expanded_ctx = test_ctx(true, false, Some(2200));
        rebuild_shell_result(
            &mut container,
            &result,
            &ToolRenderResultOptions {
                expanded: true,
                is_partial: false,
            },
            &theme,
            &expanded_ctx,
            None,
            None,
        );
        let expanded_plain = pi_rs_tui::strip_terminal_sequences(&container.render(80).join("\n"));
        assert!(expanded_plain.contains("line 1"), "{expanded_plain}");
        assert!(expanded_plain.contains("line 12"), "{expanded_plain}");
        assert!(
            !expanded_plain.contains("earlier lines"),
            "{expanded_plain}"
        );
        let _unused: Option<ToolView> = None;
    }
}
