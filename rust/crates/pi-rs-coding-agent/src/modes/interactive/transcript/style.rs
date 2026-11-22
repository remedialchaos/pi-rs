use std::borrow::Cow;

use pi_rs_tui::{strip_terminal_sequences, visible_width, wrap_text_with_ansi};

use super::Theme;

/// Transcript input can come from model output, subprocesses, or a session file.
/// Keep layout whitespace but not terminal commands from those sources.
pub(super) fn clean(text: &str) -> Cow<'_, str> {
    if !text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Cow::Borrowed(text);
    }
    let mut clean = strip_terminal_sequences(text);
    clean.retain(|c| !c.is_control() || c == '\n' || c == '\t');
    Cow::Owned(clean)
}

pub(super) fn bold(text: &str) -> String {
    format!("\x1b[1m{text}\x1b[22m")
}

pub(super) fn inner_width(width: usize, pad: usize) -> usize {
    width - 2 * pad.min(width.saturating_sub(1) / 2)
}

pub(super) fn framed(
    rows: &mut Vec<String>,
    content: Vec<String>,
    width: usize,
    pad: usize,
    paint: impl Fn(&str) -> String,
) {
    if content.is_empty() {
        return;
    }
    let pad = pad.min(width.saturating_sub(1) / 2);
    let blank = paint(&" ".repeat(width));
    rows.push(blank.clone());
    for line in content {
        let right = width.saturating_sub(pad + visible_width(&line));
        rows.push(paint(&format!("{:pad$}{line}{:right$}", "", "")));
    }
    rows.push(blank);
}

pub(super) fn text_rows(text: &str, width: usize, pad: usize) -> Vec<String> {
    if text.is_empty() || width == 0 {
        return Vec::new();
    }
    let text = if text.contains('\t') {
        Cow::Owned(text.replace('\t', "   "))
    } else {
        Cow::Borrowed(text)
    };
    let pad = pad.min(width.saturating_sub(1) / 2);
    let mut rows = wrap_text_with_ansi(&text, inner_width(width, pad));
    for line in &mut rows {
        if visible_width(line) > inner_width(width, pad) {
            // A width-one terminal cannot fit a wide grapheme.
            *line = pi_rs_tui::utils::truncate_with_ellipsis(
                line,
                inner_width(width, pad),
                "...",
                false,
            );
        }
        if pad != 0 {
            line.insert(0, ' ');
        }
    }
    rows
}

#[derive(Clone, Copy)]
pub(super) enum Keep {
    Start,
    End,
}

pub(super) fn truncate_visual(
    mut rows: Vec<String>,
    limit: usize,
    keep: Keep,
) -> (Vec<String>, usize) {
    let hidden = rows.len().saturating_sub(limit);
    if hidden > 0 {
        match keep {
            Keep::Start => rows.truncate(limit),
            Keep::End => {
                rows.drain(..hidden);
            }
        }
    }
    (rows, hidden)
}

pub(super) fn border(theme: &Theme, color: &str, width: usize) -> String {
    theme.fg(color, &"─".repeat(width))
}

pub(super) fn zone(rows: &mut [String]) {
    // OSC 133 markers identify complete user/assistant prompt zones to the terminal.
    if let Some(first) = rows.first_mut() {
        first.insert_str(0, "\x1b]133;A\x07");
    }
    if let Some(last) = rows.last_mut() {
        last.push_str("\x1b]133;B\x07\x1b]133;C\x07");
    }
}
