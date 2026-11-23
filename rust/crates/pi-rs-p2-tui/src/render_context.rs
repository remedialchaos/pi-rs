use crate::theme::ThemeRef;
use pi_rs_tui::strip_terminal_sequences;

pub trait RenderContext {
    fn theme(&self) -> &ThemeRef;
    fn key_text(&self, id: &str) -> String;
    fn request_render(&self);
}

pub fn safe_text(text: &str) -> String {
    strip_terminal_sequences(text).replace(
        |c: char| c.is_control() && c != '\n' && c != '\r' && c != '\t',
        "",
    )
}

pub fn line_text(text: &str) -> String {
    safe_text(text).replace(['\r', '\n', '\t'], " ")
}

pub fn format_key_text(key: &str, capitalize: bool) -> String {
    key.split('/')
        .map(|k| {
            k.split('+')
                .map(|part| {
                    if capitalize {
                        let mut chars = part.chars();
                        match chars.next() {
                            Some(first) => {
                                first.to_uppercase().collect::<String>() + chars.as_str()
                            }
                            None => String::new(),
                        }
                    } else {
                        part.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("+")
        })
        .collect::<Vec<_>>()
        .join("/")
}

pub fn key_hint(ctx: &dyn RenderContext, id: &str, description: &str) -> String {
    let theme = ctx.theme().current();
    format!(
        "{} {}",
        theme.fg("dim", &ctx.key_text(id)),
        theme.fg("muted", description)
    )
}

pub fn format_tokens(count: u64) -> String {
    if count < 1000 {
        count.to_string()
    } else if count < 10000 {
        format!("{:.1}k", count as f64 / 1000.0)
    } else if count < 1000000 {
        format!("{}k", (count as f64 / 1000.0).round() as u64)
    } else {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    }
}

pub fn more_lines_hint(
    ctx: &dyn RenderContext,
    _theme: &crate::theme::Theme,
    remaining: usize,
    noun: &str,
) -> String {
    format!(
        "{} (to expand)",
        key_hint(ctx, "app.tools.expand", &format!("{} {}", remaining, noun))
    )
}
