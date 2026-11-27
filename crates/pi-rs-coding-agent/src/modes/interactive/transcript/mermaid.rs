//! Terminal-native Mermaid rendering. The Markdown integration follows lovely-mermaid's
//! fence fallback: do not replace source when it cannot be represented or fitted.

mod graph;
mod other;

use std::borrow::Cow;
use std::fmt::Write as _;

use pi_rs_tui::visible_width;

use super::Theme;

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
                    line.push_str(&theme.fg(token, &super::style::bold(&text)));
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
            Ok(Some(art)) => {
                let needed = art.width();
                if needed <= width {
                    art.into_markdown(theme)
                } else if !streaming {
                    let mut original = text[span.clone()].trim_end().to_owned();
                    original.push_str("\n\n");
                    code_span(
                        &theme.fg(
                            "warning",
                            &format!("Mermaid diagram not rendered: needs {needed} columns, {width} available"),
                        ),
                        &mut original,
                    );
                    original
                } else {
                    return;
                }
            }
            Ok(None) if !streaming && width > 0 => {
                let Ok(Some(full)) = render(body, usize::MAX) else {
                    return;
                };
                let needed = full.width();
                if needed <= width {
                    return;
                }
                let mut original = text[span.clone()].trim_end().to_owned();
                original.push_str("\n\n");
                code_span(
                    &theme.fg(
                        "warning",
                        &format!("Mermaid diagram not rendered: needs {needed} columns, {width} available"),
                    ),
                    &mut original,
                );
                original
            }
            Err(warning) if !streaming => {
                let mut original = text[span.clone()].trim_end().to_owned();
                original.push_str("\n\n");
                code_span(
                    &theme.fg(
                        "warning",
                        &format!("Mermaid diagram not rendered: {warning}"),
                    ),
                    &mut original,
                );
                original
            }
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

fn render(source: &str, width: usize) -> Result<Option<Art>, &'static str> {
    // Model-generated diagrams should not induce unbounded grid allocation or parsing.
    if source.len() > 32_768 || width == 0 {
        return Ok(None);
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
        return Ok(None);
    };
    let (header, first_statement) = header.split_once(';').unwrap_or((header, ""));
    let lines = Some(first_statement)
        .filter(|statement| !statement.trim().is_empty())
        .into_iter()
        .chain(lines);
    let mut parts = header.split_whitespace();
    let kind = parts.next().unwrap_or_default();
    let mut art = match kind {
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
        "mindmap" => other::mindmap(lines)?,
        "timeline" => other::timeline(lines)?,
        "gitGraph" => other::git_graph(lines)?,
        _ => return Err("unsupported diagram type"),
    };
    if let (Some(art), Some(title)) = (&mut art, title.filter(|title| !title.is_empty())) {
        art.0.insert(0, vec![(Role::Title, title.to_owned())]);
    }
    Ok(art)
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

    #[test]
    fn native_diagrams_preserve_meaning_and_width() -> Result<(), &'static str> {
        let flow = "flowchart LR; A[Start] --> B[Done]";
        assert_eq!(
            render(flow, 80)?.map(plain).as_deref(),
            Some("┌───────┐    ┌──────┐\n│ Start ├───▶│ Done │\n└───────┘    └──────┘")
        );
        assert!(render(flow, 8)?.is_none());
        let tree = render("graph TD\n A --> B\n A --> C\n C --> A", 80)?
            .map(plain)
            .unwrap_or_default();
        assert!(tree.contains("│ A │") && tree.contains('▼') && tree.contains('│'));
        let sequence = render("sequenceDiagram\n Alice->>Bob: Hi\n Bob-->>Alice: Done", 80)?
            .map(plain)
            .unwrap_or_default();
        assert!(sequence.contains("│ Alice │  │ Bob │"));
        assert!(sequence.contains("├────────▶│") && sequence.contains("│◄╌╌╌╌╌╌╌╌┤"));
        assert!(render("flowchart Q\nA --> B", 80).is_err());
        Ok(())
    }

    #[test]
    fn code_spans_preserve_backticks_and_leading_spaces() {
        let mut row = String::new();
        code_span("   A `` B", &mut row);
        assert_eq!(row, "```    A `` B ```  \n");
    }

    #[test]
    fn subgraph_flowchart_renders_grouped_nodes_and_falls_back_when_narrow()
    -> Result<(), &'static str> {
        let diagram = concat!(
            "flowchart LR\n",
            "  subgraph Edge\n",
            "    api-gw[API Gateway]\n",
            "  end\n",
            "  subgraph Core\n",
            "    svc[Worker] & cache[Cache]\n",
            "  end\n",
            "  api-gw -- route --> svc & cache\n",
        );
        let wide = render(diagram, 80)?
            .map(plain)
            .ok_or("expected wide subgraph diagram to render")?;
        assert!(wide.contains("Edge"), "{wide}");
        assert!(wide.contains("Core"), "{wide}");
        assert!(wide.contains("API Gateway"), "{wide}");
        assert!(wide.contains("Worker"), "{wide}");
        assert!(wide.contains("Cache"), "{wide}");
        assert!(wide.contains('▶'), "{wide}");
        assert!(
            wide.lines().all(|line| visible_width(line) <= 80),
            "wide diagram exceeded viewport width:\n{wide}"
        );

        assert!(
            render(diagram, 20)?.is_none(),
            "diagram wider than 20 cells must fall back"
        );

        let fenced = format!("```mermaid\n{diagram}```");
        let theme = Theme::load(std::path::Path::new("."), Some("dark"))
            .map_err(|_| "failed to load dark theme")?;
        let narrow = transform(&theme, &fenced, 20, MermaidMode::Final, false);
        assert!(narrow.contains("subgraph Edge"));
        assert!(narrow.contains("Mermaid diagram not rendered: needs "));
        assert!(narrow.contains("20 available"));

        let narrow_streaming = transform(&theme, &fenced, 20, MermaidMode::Streaming, true);
        assert_eq!(narrow_streaming.as_ref(), fenced);

        let unclosed = "flowchart LR\n  subgraph Unclosed\n    A --> B\n";
        assert_eq!(render(unclosed, 80).err(), Some("unclosed subgraph"));
        let fenced_unclosed = format!("```mermaid\n{unclosed}```");
        let fallback_unclosed = transform(&theme, &fenced_unclosed, 80, MermaidMode::Final, false);
        assert!(fallback_unclosed.contains("subgraph Unclosed"));
        assert!(fallback_unclosed.contains("unclosed subgraph"));
        Ok(())
    }
}
