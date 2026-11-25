//! `ElanorMarkdown` rendering via `pulldown-cmark`, with p2's safety rules
//! (upstream `Markdown.tsx` + `ElanorMarkdown.tsx`): raw HTML stays text, remote images
//! become links, only web/mail links are clickable, and code blocks are escaped.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// The options upstream uses: GFM (tables, task lists, strikethrough, autolinks).
fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_GFM
        | Options::ENABLE_HEADING_ATTRIBUTES
}

/// Escapes text for HTML (upstream never injects raw HTML).
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Only web and mail addresses are clickable (upstream `safeHref`).
pub fn safe_href(href: &str) -> Option<String> {
    let trimmed = href.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("http:") || lower.starts_with("https:") || lower.starts_with("mailto:") {
        Some(trimmed.to_owned())
    } else {
        None
    }
}

/// A named character reference table for the few names upstream decodes offline.
fn named_entity(name: &str) -> Option<&'static str> {
    Some(match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => "\u{a0}",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        "hellip" => "…",
        "mdash" => "—",
        "ndash" => "–",
        "lsquo" => "‘",
        "rsquo" => "’",
        "ldquo" => "“",
        "rdquo" => "”",
        "times" => "×",
        "divide" => "÷",
        "middot" => "·",
        "bull" => "•",
        "larr" => "←",
        "rarr" => "→",
        "euro" => "€",
        "pound" => "£",
        "check" => "✓",
        _ => return None,
    })
}

/// Decodes numeric and named character references in one pass (upstream `entities`).
pub fn entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&'
            && let Some(semi) = text[i..].find(';')
        {
            let reference = &text[i + 1..i + semi];
            let decoded = if let Some(decimal) = reference.strip_prefix('#') {
                decimal
                    .parse::<u32>()
                    .ok()
                    .and_then(char::from_u32)
                    .map(|ch| ch.to_string())
            } else if let Some(hex) = reference
                .strip_prefix("#x")
                .or_else(|| reference.strip_prefix("#X"))
            {
                u32::from_str_radix(hex, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .map(|ch| ch.to_string())
            } else {
                named_entity(reference).map(str::to_owned)
            };
            if let Some(value) = decoded {
                out.push_str(&value);
                i += semi + 1;
                continue;
            }
        }
        let ch = text[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// A very small syntax highlighter for code blocks: keywords, strings, comments and numbers.
/// Emits escaped HTML only (no raw input reaches the output).
pub fn highlight_code(code: &str, language: Option<&str>) -> String {
    let _ = language; // the token classes are language-agnostic, as in the generic fallback
    let mut out = String::with_capacity(code.len());
    let chars: Vec<char> = code.chars().collect();
    let mut i = 0;
    const KEYWORDS: &[&str] = &[
        "fn", "let", "mut", "pub", "if", "else", "for", "while", "return", "const", "class", "def",
        "import", "from", "async", "await", "match", "use", "struct", "impl", "true", "false",
        "null", "None", "Some",
    ];
    while i < chars.len() {
        let rest: String = chars[i..].iter().collect();
        if rest.starts_with("//") || rest.starts_with('#') {
            let end = rest.find('\n').unwrap_or(rest.len());
            out.push_str("<span class=\"hljs-comment\">");
            out.push_str(&escape_html(&rest[..end]));
            out.push_str("</span>");
            i += rest[..end].chars().count();
            continue;
        }
        let ch = chars[i];
        if ch == '"' || ch == '\'' || ch == '`' {
            let quote = ch;
            let mut end = i + 1;
            while end < chars.len() && chars[end] != quote {
                if chars[end] == '\\' {
                    end += 1;
                }
                end += 1;
            }
            let text: String = chars[i..end.min(chars.len())].iter().collect();
            out.push_str("<span class=\"hljs-string\">");
            out.push_str(&escape_html(&text));
            out.push_str("</span>");
            i = (end + 1).min(chars.len());
            continue;
        }
        if ch.is_ascii_digit() {
            let mut end = i;
            while end < chars.len() && (chars[end].is_ascii_alphanumeric() || chars[end] == '.') {
                end += 1;
            }
            let text: String = chars[i..end].iter().collect();
            out.push_str("<span class=\"hljs-number\">");
            out.push_str(&escape_html(&text));
            out.push_str("</span>");
            i = end;
            continue;
        }
        if ch.is_alphabetic() || ch == '_' {
            let mut end = i;
            while end < chars.len() && (chars[end].is_alphanumeric() || chars[end] == '_') {
                end += 1;
            }
            let text: String = chars[i..end].iter().collect();
            if KEYWORDS.contains(&text.as_str()) {
                out.push_str("<span class=\"hljs-keyword\">");
                out.push_str(&escape_html(&text));
                out.push_str("</span>");
            } else {
                out.push_str(&escape_html(&text));
            }
            i = end;
            continue;
        }
        out.push_str(&escape_html(&ch.to_string()));
        i += 1;
    }
    out
}

/// Renders Markdown as safe HTML with Elanor-compatible classes, exactly as upstream's
/// React renderer does structurally (raw HTML escaped, unsafe links as text).
pub fn render_html(markdown: &str) -> String {
    let mut out = String::new();
    out.push_str("<div class=\"elanor-markdown\">");
    let mut in_code_block = false;
    let mut code_language: Option<String> = None;
    let mut code_buffer = String::new();
    let mut in_link = false;
    let mut heading_stack: Vec<&'static str> = vec![];
    let mut list_stack: Vec<&'static str> = vec![];
    for event in Parser::new_ext(markdown, options()) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => out.push_str("<p>"),
                Tag::Heading { level, .. } => {
                    let tag = match level {
                        HeadingLevel::H1 => "h3",
                        HeadingLevel::H2 => "h4",
                        HeadingLevel::H3 => "h5",
                        _ => "h6",
                    };
                    heading_stack.push(tag);
                    out.push_str(&format!("<{tag}>"));
                }
                Tag::BlockQuote(_) => out.push_str("<blockquote class=\"md\">"),
                Tag::CodeBlock(kind) => {
                    in_code_block = true;
                    code_buffer.clear();
                    code_language = match kind {
                        CodeBlockKind::Fenced(lang) => {
                            let first = lang.split_whitespace().next().unwrap_or("");
                            (!first.is_empty()).then(|| first.to_owned())
                        }
                        CodeBlockKind::Indented => None,
                    };
                }
                Tag::List(Some(start)) => {
                    list_stack.push("ol");
                    out.push_str(&format!("<ol start=\"{start}\">"));
                }
                Tag::List(None) => {
                    list_stack.push("ul");
                    out.push_str("<ul>");
                }
                Tag::Item => out.push_str("<li>"),
                Tag::Emphasis => out.push_str("<em>"),
                Tag::Strong => out.push_str("<strong>"),
                Tag::Strikethrough => out.push_str("<del>"),
                Tag::Link {
                    dest_url, title, ..
                } => {
                    if let Some(href) = safe_href(&dest_url) {
                        in_link = true;
                        let title_attr = if title.is_empty() {
                            String::new()
                        } else {
                            format!(" title=\"{}\"", escape_html(&title))
                        };
                        out.push_str(&format!(
                            "<a href=\"{}\" target=\"_blank\" rel=\"noreferrer\"{title_attr}>",
                            escape_html(&href)
                        ));
                    }
                }
                Tag::Image { dest_url, .. } => {
                    // Remote images are not loaded: a link named by the description.
                    if let Some(href) = safe_href(&dest_url) {
                        in_link = true;
                        out.push_str(&format!(
                            "<a href=\"{}\" target=\"_blank\" rel=\"noreferrer\">",
                            escape_html(&href)
                        ));
                    }
                }
                Tag::Table(alignments) => {
                    let classes = alignments
                        .iter()
                        .map(|a| {
                            format!(
                                "align-{}",
                                match a {
                                    pulldown_cmark::Alignment::Left => "left",
                                    pulldown_cmark::Alignment::Center => "center",
                                    pulldown_cmark::Alignment::Right => "right",
                                    pulldown_cmark::Alignment::None => "left",
                                }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    out.push_str(&format!(
                        "<div class=\"table-wrap\"><table class=\"{classes}\">"
                    ));
                }
                Tag::TableHead => out.push_str("<thead><tr>"),
                Tag::TableRow => out.push_str("<tr>"),
                Tag::TableCell => out.push_str("<td>"),
                Tag::FootnoteDefinition(_) => out.push_str("<p>"),
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => out.push_str("</p>"),
                TagEnd::Heading(_) => {
                    let tag = heading_stack.pop().unwrap_or("h6");
                    out.push_str(&format!("</{tag}>"));
                }
                TagEnd::BlockQuote(_) => out.push_str("</blockquote>"),
                TagEnd::CodeBlock => {
                    let language = code_language.take();
                    let class = language
                        .as_deref()
                        .map(|lang| format!(" class=\"language-{}\"", escape_html(lang)))
                        .unwrap_or_default();
                    let lang_attr = language
                        .as_deref()
                        .map(|lang| format!(" data-language=\"{}\"", escape_html(lang)))
                        .unwrap_or_default();
                    out.push_str(&format!(
                        "<pre{lang_attr}><code{class}>{}</code></pre>",
                        highlight_code(&code_buffer, language.as_deref())
                    ));
                    in_code_block = false;
                    code_buffer.clear();
                }
                TagEnd::List(_) => {
                    let tag = list_stack.pop().unwrap_or("ul");
                    out.push_str(&format!("</{tag}>"));
                }
                TagEnd::Item => out.push_str("</li>"),
                TagEnd::Emphasis => out.push_str("</em>"),
                TagEnd::Strong => out.push_str("</strong>"),
                TagEnd::Strikethrough => out.push_str("</del>"),
                TagEnd::Link | TagEnd::Image => {
                    if in_link {
                        out.push_str("</a>");
                        in_link = false;
                    }
                }
                TagEnd::Table => out.push_str("</table></div>"),
                TagEnd::TableHead => out.push_str("</tr></thead><tbody>"),
                TagEnd::TableRow => out.push_str("</tr>"),
                TagEnd::TableCell => out.push_str("</td>"),
                TagEnd::FootnoteDefinition => out.push_str("</p>"),
                _ => {}
            },
            Event::Text(text) => {
                if in_code_block {
                    code_buffer.push_str(&text);
                } else {
                    out.push_str(&escape_html(&entities(&text)));
                }
            }
            Event::Code(text) => out.push_str(&format!("<code>{}</code>", escape_html(&text))),
            Event::Html(text) | Event::InlineHtml(text) => out.push_str(&escape_html(&text)),
            Event::SoftBreak => {
                if in_code_block {
                    code_buffer.push('\n');
                } else {
                    out.push_str("<br />");
                }
            }
            Event::HardBreak => {
                if in_code_block {
                    code_buffer.push('\n');
                } else {
                    out.push_str("<br />");
                }
            }
            Event::TaskListMarker(checked) => {
                out.push_str(if checked {
                    "<span class=\"markdown-checkbox\" data-checked=\"true\" aria-label=\"Done\">✓</span>"
                } else {
                    "<span class=\"markdown-checkbox\" data-checked=\"false\" aria-label=\"Not done\"></span>"
                });
            }
            Event::Rule => out.push_str("<hr />"),
            Event::InlineMath(text) | Event::DisplayMath(text) => out.push_str(&escape_html(&text)),
            Event::FootnoteReference(label) => out.push_str(&escape_html(&label)),
        }
    }
    out.push_str("</div>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_raw_html_and_script() {
        let html = render_html("hello <script>alert(1)</script> & <b>bold</b>");
        assert!(html.contains("&lt;script&gt;"));
        assert!(!html.contains("<script>"));
        assert!(html.contains("&amp;"));
    }

    #[test]
    fn only_safe_links_are_clickable() {
        let html = render_html("[ok](https://example.com) [bad](javascript:alert(1))");
        assert!(html.contains("href=\"https://example.com\""));
        assert!(html.contains("rel=\"noreferrer\""));
        assert!(!html.contains("javascript:"));
        assert_eq!(html.matches("href=").count(), 1);
    }

    #[test]
    fn code_blocks_are_escaped_and_labelled() {
        let html = render_html("```rust\nfn main() { let x = 1; }\n```");
        assert!(html.contains("data-language=\"rust\""));
        assert!(html.contains("hljs-keyword"));
        assert!(!html.contains("<script>"));
    }

    #[test]
    fn headings_start_at_h3() {
        let html = render_html("# Title\n## Sub\n### More");
        assert!(html.contains("<h3>Title</h3>"));
        assert!(html.contains("<h4>Sub</h4>"));
        assert!(html.contains("<h5>More</h5>"));
    }

    #[test]
    fn tables_and_task_lists_render() {
        let html = render_html("| a | b |\n| --- | --- |\n| 1 | 2 |\n\n- [x] done\n- [ ] todo");
        assert!(html.contains("<table"));
        assert!(html.contains("markdown-checkbox"));
        assert!(html.contains("data-checked=\"true\""));
    }

    #[test]
    fn entities_decode_and_images_become_links() {
        assert_eq!(entities("a &amp; b &#38; c &copy;"), "a & b & c ©");
        let html = render_html("![alt](https://img.example/x.png)");
        assert!(html.contains("href=\"https://img.example/x.png\""));
        assert!(!html.contains("<img"));
    }
}
