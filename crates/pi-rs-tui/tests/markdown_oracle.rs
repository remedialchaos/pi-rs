use std::io;
use std::rc::Rc;

use pi_rs_tui::{
    Component, DefaultTextStyle, Markdown, MarkdownOptions, MarkdownTheme, TerminalCapabilities,
    set_capabilities, strip_terminal_sequences,
};
use serde_json::Value;

fn oracle_theme() -> MarkdownTheme {
    let ansi = |open: &'static str, close: &'static str| -> pi_rs_tui::components::TextStyleFn {
        Rc::new(move |text| {
            if text.is_empty() {
                String::new()
            } else {
                format!(
                    "{open}{}{close}",
                    text.replace(close, &format!("{close}{open}"))
                )
            }
        })
    };
    MarkdownTheme {
        heading: ansi("\x1b[1m\x1b[36m", "\x1b[39m\x1b[22m"),
        link: ansi("\x1b[34m", "\x1b[39m"),
        link_url: ansi("\x1b[2m", "\x1b[22m"),
        code: ansi("\x1b[33m", "\x1b[39m"),
        code_block: ansi("\x1b[32m", "\x1b[39m"),
        code_block_border: ansi("\x1b[2m", "\x1b[22m"),
        quote: ansi("\x1b[3m", "\x1b[23m"),
        quote_border: ansi("\x1b[2m", "\x1b[22m"),
        hr: ansi("\x1b[2m", "\x1b[22m"),
        list_bullet: ansi("\x1b[36m", "\x1b[39m"),
        ..Default::default()
    }
}

#[test]
fn upstream_markdown_frames_match_plain_terminal_rows() -> io::Result<()> {
    let mut differences = Vec::new();
    for line in include_str!("fixtures/markdown.jsonl").lines() {
        let case: Value = serde_json::from_str(line).map_err(io::Error::other)?;
        let missing = || io::Error::other("malformed Markdown oracle fixture");
        let source = case["source"].as_str().ok_or_else(missing)?;
        let number = |value: &Value| {
            value
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(missing)
        };
        let width = number(&case["width"])?;
        let x = number(&case["geometry"][0])?;
        let y = number(&case["geometry"][1])?;
        let hyperlinks = case["hyperlinks"].as_bool().ok_or_else(missing)?;
        set_capabilities(TerminalCapabilities {
            hyperlinks,
            ..Default::default()
        });
        let options = MarkdownOptions {
            render_latex: case["renderLatex"].as_bool().unwrap_or(true),
            preserve_ordered_list_markers: case["preserveOrderedListMarkers"]
                .as_bool()
                .unwrap_or(false),
            preserve_backslash_escapes: case["preserveBackslashEscapes"].as_bool().unwrap_or(false),
            transform: None,
        };
        let markdown = Markdown::new(
            source,
            x,
            y,
            oracle_theme(),
            DefaultTextStyle::default(),
            options,
        );
        let actual: Vec<_> = markdown
            .render(width)
            .iter()
            .map(|line| strip_terminal_sequences(line).trim_end().to_owned())
            .collect();
        let expected: Vec<_> = case["lines"]
            .as_array()
            .ok_or_else(missing)?
            .iter()
            .map(|line| line.as_str().ok_or_else(missing))
            .collect::<io::Result<_>>()?;
        if actual != expected {
            differences.push(format!(
                "source={source:?} width={width}\n  expected {expected:?}\n  actual   {actual:?}"
            ));
        }
    }
    assert!(
        differences.is_empty(),
        "{} Markdown oracle mismatches:\n{}",
        differences.len(),
        differences.join("\n")
    );
    Ok(())
}
