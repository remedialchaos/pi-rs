use crate::preferences::TuiSettings;
use crate::theme::Theme;

#[derive(Debug, Clone)]
pub struct MermaidTransformContext {
    pub settings: Option<TuiSettings>,
    pub message_type: String,
    pub is_streaming: bool,
    pub available_width: usize,
    pub theme: Theme,
}

mod graph;
mod other;

use std::borrow::Cow;
use std::fmt::Write as _;

use pi_rs_tui::visible_width;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MermaidMode {
    Off,
    Final,
    Streaming,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Border,
    Text,
    Edge,
    Title,
}

#[derive(Default)]
struct Art(Vec<Vec<(Role, String)>>);

enum RenderOutcome {
    Empty,
    Rendered(Art),
    TooWide { needed: usize },
}

impl From<Option<Art>> for RenderOutcome {
    fn from(value: Option<Art>) -> Self {
        match value {
            Some(art) => Self::Rendered(art),
            None => Self::Empty,
        }
    }
}

impl Art {
    fn push(&mut self, role: Role, text: impl Into<String>) {
        self.0.push(vec![(role, text.into())]);
    }

    fn width(&self) -> usize {
        self.0
            .iter()
            .map(|row| row.iter().map(|(_, text)| visible_width(text)).sum())
            .max()
            .unwrap_or(0)
    }

    fn into_markdown(self, theme: &Theme) -> String {
        let mut result = String::new();
        for row in self.0 {
            let mut line = String::new();
            for (role, text) in row {
                let token = match role {
                    Role::Border => "borderMuted",
                    Role::Text => "text",
                    Role::Edge | Role::Title => "accent",
                };
                if role == Role::Title {
                    line.push_str(&theme.fg(token, &theme.bold(&text)));
                } else {
                    line.push_str(&theme.fg(token, &text));
                }
            }
            code_span(&line, &mut result);
        }
        result
    }
}

fn code_span(line: &str, out: &mut String) {
    let line = if line.is_empty() { "\u{a0}" } else { line };
    let delimiter = "`".repeat(line.split(|c| c != '`').map(str::len).max().unwrap_or(0) + 1);
    let _ = writeln!(out, "{delimiter} {line} {delimiter}  ");
}

fn unrendered_note(theme: &Theme, raw: &str, note: &str) -> String {
    let mut original = raw.trim_end().to_owned();
    original.push_str("\n\n");
    code_span(&theme.fg("warning", note), &mut original);
    original
}

#[derive(Clone, Copy)]
struct Fence {
    marker: u8,
    count: usize,
    indent: usize,
}

impl Fence {
    fn parse(line: &str) -> Option<(Self, &str)> {
        let indent = line.bytes().take_while(|&byte| byte == b' ').count();
        if indent > 3 {
            return None;
        }
        let first = *line.as_bytes().get(indent)?;
        if !matches!(first, b'`' | b'~') {
            return None;
        }
        let count = line[indent..]
            .bytes()
            .take_while(|&byte| byte == first)
            .count();
        (count >= 3).then(|| {
            (
                Self {
                    marker: first,
                    count,
                    indent,
                },
                line[indent + count..].trim(),
            )
        })
    }
}

pub(super) fn transform<'a>(
    theme: &Theme,
    text: &'a str,
    width: usize,
    mode: MermaidMode,
    streaming: bool,
) -> Cow<'a, str> {
    if mode == MermaidMode::Off || (streaming && mode == MermaidMode::Final) {
        return Cow::Borrowed(text);
    }
    let mut open = None;
    let mut position = 0;
    let mut copied = 0;
    let mut transformed = String::new();
    let mut list_indent = None;
    let mut replace = |span: std::ops::Range<usize>, body: &str| {
        let replacement = match render(body, width) {
            Ok(RenderOutcome::Rendered(art)) => art.into_markdown(theme),
            Ok(RenderOutcome::TooWide { needed }) if !streaming => unrendered_note(
                theme,
                &text[span.clone()],
                &format!("Mermaid diagram not rendered: needs {needed} columns, {width} available"),
            ),
            Err(warning) if !streaming => unrendered_note(
                theme,
                &text[span.clone()],
                &format!("Mermaid diagram not rendered: {warning}"),
            ),
            _ => return,
        };
        transformed.push_str(&text[copied..span.start]);
        if !transformed.is_empty() && !transformed.ends_with("\n\n") {
            transformed.push('\n');
        }
        transformed.push_str(&replacement);
        transformed.push('\n');
        copied = span.end;
    };
    for line in text.split_inclusive('\n') {
        if let Some((active, start, body, diagram)) = open {
            let active: Fence = active;
            if let Some((fence, rest)) = Fence::parse(line)
                && active.marker == fence.marker
                && fence.count >= active.count
                && rest.is_empty()
            {
                if diagram {
                    replace(start..position + line.len(), &text[body..position]);
                }
                open = None;
            }
        } else {
            let line_indent = line.bytes().take_while(|&byte| byte == b' ').count();
            if !line.trim().is_empty() && list_indent.is_some_and(|indent| line_indent < indent) {
                list_indent = None;
            }
            if let Some(indent) = list_content_indent(line) {
                list_indent = Some(indent);
            }
            if let Some((fence, info)) = Fence::parse(line)
                && (fence.marker != b'`' || !info.contains('`'))
            {
                let diagram = list_indent.is_none_or(|indent| fence.indent < indent)
                    && info
                        .split_whitespace()
                        .next()
                        .is_some_and(|lang| lang.eq_ignore_ascii_case("mermaid"));
                open = Some((fence, position, position + line.len(), diagram));
            }
        }
        position += line.len();
    }
    if let Some((_, start, body, true)) = open {
        replace(start..text.len(), &text[body..]);
    }
    if copied == 0 {
        Cow::Borrowed(text)
    } else {
        transformed.push_str(&text[copied..]);
        Cow::Owned(transformed)
    }
}

fn list_content_indent(line: &str) -> Option<usize> {
    let trimmed = line.trim_start_matches(' ');
    let prefix = line.len() - trimmed.len();
    let marker = trimmed.as_bytes().first()?;
    let length = if matches!(marker, b'-' | b'+' | b'*') {
        1
    } else {
        let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || !matches!(trimmed.as_bytes().get(digits), Some(b'.' | b')')) {
            return None;
        }
        digits + 1
    };
    trimmed
        .as_bytes()
        .get(length)
        .is_some_and(u8::is_ascii_whitespace)
        .then_some(prefix + length + 1)
}

fn render(source: &str, width: usize) -> Result<RenderOutcome, &'static str> {
    // Model-generated diagrams should not induce unbounded grid allocation or parsing.
    if source.len() > 32_768 || width == 0 {
        return Ok(RenderOutcome::Empty);
    }
    let mut source = source.trim_start();
    let mut title = None;
    if let Some(front) = source.strip_prefix("---\n") {
        let (front, rest) = front.split_once("\n---\n").ok_or("unclosed frontmatter")?;
        title = front.lines().find_map(|line| {
            line.trim_start()
                .strip_prefix("title:")
                .map(|title| title.trim().trim_matches(['\'', '"']))
        });
        source = rest;
    }
    let mut lines = source.lines().filter(|line| {
        let line = line.trim_start();
        !line.is_empty() && !line.starts_with("%%")
    });
    let Some(header) = lines.next() else {
        return Ok(RenderOutcome::Empty);
    };
    let (header, first_statement) = header.split_once(';').unwrap_or((header, ""));
    let lines = Some(first_statement)
        .filter(|statement| !statement.trim().is_empty())
        .into_iter()
        .chain(lines);
    let mut parts = header.split_whitespace();
    let kind = parts.next().unwrap_or_default();
    let mut outcome = match kind {
        "flowchart" | "graph" => {
            graph::render(parts.next(), lines, width, graph::Grammar::Flowchart)?
        }
        "stateDiagram" | "stateDiagram-v2" => {
            graph::render(None, lines, width, graph::Grammar::State)?
        }
        "classDiagram" => graph::render(None, lines, width, graph::Grammar::Class)?,
        "erDiagram" => graph::render(None, lines, width, graph::Grammar::Er)?,
        "sequenceDiagram" => other::sequence(lines, width)?,
        "pie" => other::pie(header, lines, width)?,
        "mindmap" => other::mindmap(lines)?.into(),
        "timeline" => other::timeline(lines)?.into(),
        "gitGraph" => other::git_graph(lines)?.into(),
        _ => return Err("unsupported diagram type"),
    };
    if let RenderOutcome::Rendered(art) = &mut outcome {
        if let Some(title) = title.filter(|title| !title.is_empty()) {
            art.0.insert(0, vec![(Role::Title, title.to_owned())]);
        }
        let needed = art.width();
        if needed > width {
            return Ok(RenderOutcome::TooWide { needed });
        }
    }
    Ok(outcome)
}

pub fn mermaid_markdown_transformer(markdown: &str, context: MermaidTransformContext) -> String {
    let setting = context
        .settings
        .as_ref()
        .and_then(|s| s.markdown.as_ref())
        .and_then(|m| m.mermaid.as_deref());
    let mode = match setting {
        _ if context.message_type == "assistant-thinking" => MermaidMode::Off,
        Some("off") => MermaidMode::Off,
        Some("final") => MermaidMode::Final,
        _ => MermaidMode::Streaming,
    };
    transform(
        &context.theme,
        markdown,
        context.available_width,
        mode,
        context.is_streaming,
    )
    .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(art: Art) -> String {
        art.0
            .into_iter()
            .map(|line| line.into_iter().map(|(_, text)| text).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn rendered_art(outcome: RenderOutcome) -> Option<Art> {
        match outcome {
            RenderOutcome::Rendered(art) => Some(art),
            RenderOutcome::Empty | RenderOutcome::TooWide { .. } => None,
        }
    }

    #[test]
    fn native_diagrams_preserve_meaning_and_width() -> Result<(), &'static str> {
        let flow = "flowchart LR; A[Start] --> B[Done]";
        assert_eq!(
            rendered_art(render(flow, 80)?).map(plain).as_deref(),
            Some("┌───────┐    ┌──────┐\n│ Start ├───▶│ Done │\n└───────┘    └──────┘")
        );
        assert!(matches!(
            render(flow, 8)?,
            RenderOutcome::TooWide { needed: 21 }
        ));
        let tree = rendered_art(render("graph TD\n A --> B\n A --> C\n C --> A", 80)?)
            .map(plain)
            .unwrap_or_default();
        assert!(tree.contains("│ A │") && tree.contains('▼') && tree.contains('│'));
        let sequence = rendered_art(render(
            "sequenceDiagram\n Alice->>Bob: Hi\n Bob-->>Alice: Done",
            80,
        )?)
        .map(plain)
        .unwrap_or_default();
        assert!(sequence.contains("│ Alice │  │ Bob │"));
        assert!(sequence.contains("├────────▶│") && sequence.contains("│◄╌╌╌╌╌╌╌╌┤"));
        assert!(render("flowchart Q\nA --> B", 80).is_err());
        Ok(())
    }

    #[test]
    fn oversized_mermaid_explains_needed_columns_only_after_streaming() {
        let theme = Theme::default();
        let md = "```mermaid\nflowchart LR\n  A[Start] --> B[Finish]\n```\n";
        let streaming = transform(&theme, md, 8, MermaidMode::Streaming, true);
        assert_eq!(streaming, md);

        let done = transform(&theme, md, 8, MermaidMode::Streaming, false);
        assert!(done.contains("```mermaid\nflowchart LR\n  A[Start] --> B[Finish]\n```"));
        assert!(done.contains("Mermaid diagram not rendered: needs 23 columns, 8 available"));

        // Invalid syntax keeps the warning note rather than reporting width, even when narrow.
        let invalid =
            "```mermaid\nflowchart Q\n  A[VeryLongNodeName] --> B[AnotherLongNodeName]\n```\n";
        let invalid_done = transform(&theme, invalid, 8, MermaidMode::Streaming, false);
        assert!(
            invalid_done.contains("Mermaid diagram not rendered: unsupported flowchart direction")
        );
        assert!(!invalid_done.contains("columns"));

        let seq = "```mermaid\nsequenceDiagram\n  Alice->>Bob: Hello\n```\n";
        let seq_done = transform(&theme, seq, 8, MermaidMode::Final, false);
        assert!(seq_done.contains("Mermaid diagram not rendered: needs "));
        assert!(seq_done.contains("8 available"));

        let pie = "```mermaid\npie\n  \"Alpha\" : 60\n  \"Beta\" : 40\n```\n";
        let pie_done = transform(&theme, pie, 10, MermaidMode::Final, false);
        assert!(pie_done.contains("Mermaid diagram not rendered: needs 24 columns, 10 available"));
    }

    #[test]
    fn code_spans_preserve_backticks_and_leading_spaces() {
        let mut row = String::new();
        code_span("   A `` B", &mut row);
        assert_eq!(row, "```    A `` B ```  \n");
    }
}
