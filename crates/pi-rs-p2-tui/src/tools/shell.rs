use crate::components::visual_truncate::VisualLinePreview;
use crate::render_context::safe_text;
use crate::theme::Theme;
use crate::tools::render_utils::{get_text_output, invalid_arg_text, num_value, str_value};
use crate::tools::types::{
    ToolArgs, ToolRenderContext, ToolRenderResultOptions, ToolRenderers, ToolResultView,
};
use pi_rs_tui::{Container, Spacer, Text};

const SHELL_PREVIEW_LINES: usize = 5;

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

fn format_shell_call(args: &ToolArgs, prompt: &str, theme: &Theme) -> String {
    let command = args.get("command").and_then(str_value).unwrap_or_default();
    let timeout = args.get("timeout").and_then(num_value);
    let timeout_suffix = timeout
        .map(|t| theme.fg("muted", &format!(" (timeout {}s)", t as u64)))
        .unwrap_or_default();
    let command_display = if command.is_empty() {
        invalid_arg_text(theme)
    } else {
        theme.fg("toolOutput", &safe_text(&command))
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

fn rebuild_shell_result(
    component: &mut Container,
    result: &ToolResultView,
    options: &ToolRenderResultOptions,
    theme: &Theme,
    context: &ToolRenderContext,
    started_at: Option<u64>,
    ended_at: Option<u64>,
) {
    component.clear();
    let output = get_text_output(Some(result)).trim().to_string();

    if !output.is_empty() {
        let preview = VisualLinePreview::new(output, SHELL_PREVIEW_LINES, 80, 0);
        for line in preview.render(80) {
            component.add_child(pi_rs_tui::ComponentRef::new(Text::new(line, 0, 0, None)));
        }
    }

    if !options.is_partial {
        if let Some(duration) = context.duration_ms {
            component.add_child(pi_rs_tui::ComponentRef::new(Spacer::new(1)));
            component.add_child(pi_rs_tui::ComponentRef::new(Text::new(
                theme.fg("muted", &format!("\nTook {}", format_duration(duration))),
                0,
                0,
                None,
            )));
        }
    } else if let Some(started) = started_at {
        let elapsed = ended_at
            .unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64
            })
            .saturating_sub(started);
        component.add_child(pi_rs_tui::ComponentRef::new(Spacer::new(1)));
        component.add_child(pi_rs_tui::ComponentRef::new(Text::new(
            theme.fg(
                "muted",
                &format!("\nRunning for {}", format_duration(elapsed)),
            ),
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
