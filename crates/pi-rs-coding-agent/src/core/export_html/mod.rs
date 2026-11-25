//! Self-contained HTML export of a coding-agent session.
//!
//! Port of `packages/coding-agent/src/core/export-html/`. Upstream stores the
//! session as base64 JSON and renders it in the browser with vendored `marked`,
//! `marked-cjk-friendly`, and `highlight.js` scripts. This port renders the same
//! transcript on the server: CSS is inlined, code highlighting uses
//! [`crate::utils::syntax_highlight`], and no external assets are emitted.
//!
//! The public entry point is [`export_session_html`]. A session is supplied as a
//! [`SessionSnapshot`], which is built from a `serde_json::Value` (either the
//! upstream `{ header, entries, leafId, ... }` shape or a bare array of entries)
//! or from JSONL text.

pub mod styles;
pub mod template;

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use fancy_regex::Regex;
use indexmap::IndexSet;
use serde_json::Value;

use crate::utils::syntax_highlight;

/// Escape `&` first so entities introduced by later replacements are not
/// escaped a second time. `ansi_escape_html` keeps upstream's `&#039;`
/// apostrophe entity; the document renderer uses `&#39;`.
fn escape_html_ordered(text: &str, apostrophe: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', apostrophe)
}

/// Escape text for HTML text nodes and double-quoted attributes.
pub fn escape_html(text: &str) -> String {
    escape_html_ordered(text, "&#39;")
}

/// Escape text for the ANSI converter, which emits `&#039;` upstream.
pub fn escape_html_ansi(text: &str) -> String {
    escape_html_ordered(text, "&#039;")
}

// ---------------------------------------------------------------------------
// ANSI to HTML
// ---------------------------------------------------------------------------

const ANSI_COLORS: [&str; 16] = [
    "#000000", "#800000", "#008000", "#808000", "#000080", "#800080", "#008080", "#c0c0c0",
    "#808080", "#ff0000", "#00ff00", "#ffff00", "#0000ff", "#ff00ff", "#00ffff", "#ffffff",
];

#[derive(Debug, Clone, Default)]
struct TextStyle {
    fg: Option<String>,
    bg: Option<String>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
}

impl TextStyle {
    fn is_empty(&self) -> bool {
        self.fg.is_none()
            && self.bg.is_none()
            && !self.bold
            && !self.dim
            && !self.italic
            && !self.underline
    }

    fn inline_css(&self) -> String {
        let mut parts = Vec::new();
        if let Some(fg) = &self.fg {
            parts.push(format!("color:{fg}"));
        }
        if let Some(bg) = &self.bg {
            parts.push(format!("background-color:{bg}"));
        }
        if self.bold {
            parts.push("font-weight:bold".to_string());
        }
        if self.dim {
            parts.push("opacity:0.6".to_string());
        }
        if self.italic {
            parts.push("font-style:italic".to_string());
        }
        if self.underline {
            parts.push("text-decoration:underline".to_string());
        }
        parts.join(";")
    }

    fn apply_sgr(&mut self, params: &[u32]) {
        let mut i = 0;
        while i < params.len() {
            match params[i] {
                0 => *self = Self::default(),
                1 => self.bold = true,
                2 => self.dim = true,
                3 => self.italic = true,
                4 => self.underline = true,
                22 => {
                    self.bold = false;
                    self.dim = false;
                }
                23 => self.italic = false,
                24 => self.underline = false,
                code @ 30..=37 => self.fg = Some(ANSI_COLORS[(code - 30) as usize].to_string()),
                38 => {
                    if params.get(i + 1) == Some(&5)
                        && let Some(index) = params.get(i + 2)
                    {
                        self.fg = Some(color256_to_hex(*index));
                        i += 2;
                    } else if params.get(i + 1) == Some(&2) && params.len() > i + 4 {
                        self.fg = Some(format!(
                            "rgb({},{},{})",
                            params[i + 2],
                            params[i + 3],
                            params[i + 4]
                        ));
                        i += 4;
                    }
                }
                39 => self.fg = None,
                code @ 40..=47 => self.bg = Some(ANSI_COLORS[(code - 40) as usize].to_string()),
                48 => {
                    if params.get(i + 1) == Some(&5)
                        && let Some(index) = params.get(i + 2)
                    {
                        self.bg = Some(color256_to_hex(*index));
                        i += 2;
                    } else if params.get(i + 1) == Some(&2) && params.len() > i + 4 {
                        self.bg = Some(format!(
                            "rgb({},{},{})",
                            params[i + 2],
                            params[i + 3],
                            params[i + 4]
                        ));
                        i += 4;
                    }
                }
                49 => self.bg = None,
                code @ 90..=97 => self.fg = Some(ANSI_COLORS[(code - 90 + 8) as usize].to_string()),
                code @ 100..=107 => {
                    self.bg = Some(ANSI_COLORS[(code - 100 + 8) as usize].to_string())
                }
                _ => {}
            }
            i += 1;
        }
    }
}

fn color256_to_hex(index: u32) -> String {
    if index < 16 {
        return ANSI_COLORS[index as usize].to_string();
    }
    if index < 232 {
        let cube = index - 16;
        let to_hex = |level: u32| {
            let value = if level == 0 { 0 } else { 55 + level * 40 };
            format!("{value:02x}")
        };
        return format!(
            "#{}{}{}",
            to_hex(cube / 36),
            to_hex((cube % 36) / 6),
            to_hex(cube % 6),
        );
    }
    let gray = 8 + (index - 232) * 10;
    format!("#{gray:02x}{gray:02x}{gray:02x}")
}

/// SGR, other CSI, OSC (BEL or ST terminated) and APC sequences, mirroring
/// upstream's `ANSI_REGEX`.
const ANSI_PATTERN: &str = concat!(
    r"\x1b\[([\d;]*)m|",
    r"\x1b\[[0-?]*[ -/]*[@-~]|",
    r"\x1b\]([^\x07\x1b]*)(?:\x07|\x1b\\)|",
    r"\x1b_[^\x07\x1b]*(?:\x07|\x1b\\)"
);

fn ansi_regex() -> Option<&'static Regex> {
    static REGEX: OnceLock<Option<Regex>> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(ANSI_PATTERN).ok()).as_ref()
}

/// Remove all escape sequences and stray ESC characters.
pub fn strip_ansi_escapes(text: &str) -> String {
    let stripped = match ansi_regex() {
        Some(regex) => regex.replace_all(text, "").into_owned(),
        None => text.to_string(),
    };
    stripped.replace('\u{1b}', "")
}

fn parse_osc8(payload: &str) -> Option<String> {
    let rest = payload.strip_prefix("8;")?;
    let (_, url) = rest.split_once(';')?;
    let cleaned: String = url
        .trim()
        .chars()
        .filter(|ch| *ch as u32 > 0x1f && *ch != '\u{7f}')
        .collect();
    Some(cleaned)
}

fn is_safe_link(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("http:") || lower.starts_with("https:") || lower.starts_with("mailto:")
}

/// Convert SGR-styled ANSI text and OSC 8 hyperlinks to HTML. Unsafe link
/// schemes and all other escape sequences are dropped; their text is kept.
pub fn ansi_to_html(text: &str) -> String {
    let mut style = TextStyle::default();
    let mut result = String::new();
    let mut last_index = 0;
    let mut in_span = false;
    let mut in_link = false;

    let close_span = |result: &mut String, in_span: &mut bool| {
        if *in_span {
            result.push_str("</span>");
            *in_span = false;
        }
    };
    let open_span = |result: &mut String, in_span: &mut bool, style: &TextStyle| {
        if !style.is_empty() {
            result.push_str("<span style=\"");
            result.push_str(&style.inline_css());
            result.push_str("\">");
            *in_span = true;
        }
    };

    let Some(regex) = ansi_regex() else {
        return escape_html_ansi(&text.replace('\u{1b}', ""));
    };
    for captures in regex.captures_iter(text).flatten() {
        let Some(full) = captures.get(0) else {
            continue;
        };
        let before = &text[last_index..full.start()];
        if !before.is_empty() {
            result.push_str(&escape_html_ansi(&before.replace('\u{1b}', "")));
        }

        if let Some(sgr) = captures.get(1) {
            let params: Vec<u32> = if sgr.as_str().is_empty() {
                vec![0]
            } else {
                sgr.as_str()
                    .split(';')
                    .map(|param| param.parse::<u32>().unwrap_or(0))
                    .collect()
            };
            close_span(&mut result, &mut in_span);
            style.apply_sgr(&params);
            open_span(&mut result, &mut in_span, &style);
        } else if let Some(osc) = captures.get(2)
            && let Some(url) = parse_osc8(osc.as_str())
        {
            close_span(&mut result, &mut in_span);
            if in_link {
                result.push_str("</a>");
                in_link = false;
            }
            if is_safe_link(&url) {
                result.push_str("<a href=\"");
                result.push_str(&escape_html_ansi(&url));
                result.push_str("\">");
                in_link = true;
            }
            open_span(&mut result, &mut in_span, &style);
        }
        last_index = full.end();
    }

    let remaining = &text[last_index..];
    if !remaining.is_empty() {
        result.push_str(&escape_html_ansi(&remaining.replace('\u{1b}', "")));
    }
    close_span(&mut result, &mut in_span);
    if in_link {
        result.push_str("</a>");
    }
    result
}

/// Convert ANSI lines into `<div class="ansi-line">` rows without inserting
/// template whitespace between them. Blank lines use `&nbsp;`.
pub fn ansi_lines_to_html(lines: &[String]) -> String {
    let mut html = String::with_capacity(lines.len() * 48);
    for line in lines {
        let rendered = ansi_to_html(line);
        html.push_str("<div class=\"ansi-line\">");
        if rendered.is_empty() {
            html.push_str("&nbsp;");
        } else {
            html.push_str(&rendered);
        }
        html.push_str("</div>");
    }
    html
}

// ---------------------------------------------------------------------------
// CJK-friendly markdown
// ---------------------------------------------------------------------------

fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3000..=0x303f
            | 0x3040..=0x30ff
            | 0x3400..=0x4dbf
            | 0x4e00..=0x9fff
            | 0xac00..=0xd7af
            | 0xf900..=0xfaff
            | 0xff00..=0xffef
            | 0x20000..=0x2fa1f
    )
}

/// CJK characters and punctuation count as emphasis boundaries, which is what
/// `marked-cjk-friendly` relaxes compared with CommonMark flanking rules.
fn is_emphasis_boundary(ch: char) -> bool {
    is_cjk(ch) || ch.is_ascii_punctuation() || ch.is_whitespace() || !ch.is_alphanumeric()
}

fn sanitize_markdown_url(value: &str) -> Option<String> {
    let cleaned: String = value
        .trim()
        .chars()
        .filter(|ch| *ch as u32 > 0x1f && *ch != '\u{7f}')
        .collect();
    if cleaned.is_empty() {
        return Some(cleaned);
    }
    let scheme: String = cleaned
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
        .collect();
    if !scheme.is_empty()
        && cleaned
            .get(scheme.len()..)
            .is_some_and(|rest| rest.starts_with(':'))
        && !["http", "https", "mailto", "tel", "ftp"]
            .iter()
            .any(|allowed| scheme.eq_ignore_ascii_case(allowed))
    {
        return None;
    }
    Some(cleaned)
}

fn push_placeholder(placeholders: &mut Vec<String>, html: String) -> String {
    let index = placeholders.len();
    placeholders.push(html);
    format!("\u{e000}{index}\u{e001}")
}

/// Length of the delimiter run starting at `start`.
fn run_length(chars: &[char], start: usize) -> usize {
    let delimiter = chars[start];
    let mut end = start;
    while end < chars.len() && chars[end] == delimiter {
        end += 1;
    }
    end - start
}

/// Find the first run of `delimiter` with exactly `len` characters at or after
/// `from`, returning its start and end offsets.
fn find_run_of_length(
    chars: &[char],
    from: usize,
    delimiter: char,
    len: usize,
) -> Option<(usize, usize)> {
    let mut index = from;
    while index < chars.len() {
        if chars[index] == delimiter {
            let end = index + run_length(chars, index);
            if end - index == len {
                return Some((index, end));
            }
            index = end;
        } else {
            index += 1;
        }
    }
    None
}

/// Check the closing side of an emphasis run: the character before it must not
/// be whitespace and the character after it must be whitespace, punctuation or
/// a CJK character (the relaxed `marked-cjk-friendly` rule).
fn emphasis_closer(
    chars: &[char],
    start: usize,
    len: usize,
    delimiter: char,
) -> Option<(usize, usize)> {
    let prev = start.checked_sub(1).map(|index| chars[index]);
    let next = chars.get(start + len).copied();
    if (0..len).all(|offset| chars.get(start + offset) == Some(&delimiter))
        && prev.is_some_and(|ch| !ch.is_whitespace())
        && next.is_none_or(|ch| ch.is_whitespace() || is_emphasis_boundary(ch))
    {
        Some((start, start + len))
    } else {
        None
    }
}

fn emphasis_pass(chars: &[char], mut pos: usize, depth: usize, out: &mut String) -> usize {
    while pos < chars.len() {
        if depth < 8 && chars[pos] == '~' {
            let len = run_length(chars, pos);
            if len == 2
                && let Some((start, end)) = find_run_of_length(chars, pos + 2, '~', 2)
                && emphasis_closer(chars, start, 2, '~').is_some()
            {
                let inner: String = chars[pos + 2..start].iter().collect();
                if !inner.is_empty() && inner.trim() == inner && !inner.contains('~') {
                    let mut rendered = String::new();
                    emphasis_pass(&chars[pos + 2..start], 0, depth + 1, &mut rendered);
                    out.push_str(&format!("<del>{rendered}</del>"));
                    pos = end;
                    continue;
                }
            }
        }
        if depth < 8 && (chars[pos] == '*' || chars[pos] == '_') {
            let delimiter = chars[pos];
            let len = run_length(chars, pos);
            let next = chars.get(pos + len).copied();
            let prev = pos.checked_sub(1).map(|index| chars[index]);
            let opener_ok = next.is_some_and(|ch| !ch.is_whitespace())
                && prev.is_none_or(|ch| {
                    ch.is_whitespace()
                        || is_emphasis_boundary(ch)
                        || (delimiter == '*' && is_cjk(ch))
                });
            if opener_ok
                && let Some((start, end)) = find_run_of_length(chars, pos + len, delimiter, len)
                && emphasis_closer(chars, start, len, delimiter).is_some()
            {
                let inner: Vec<char> = chars[pos + len..start].to_vec();
                let tag = if len >= 2 { "strong" } else { "em" };
                let mut rendered = String::new();
                emphasis_pass(&inner, 0, depth + 1, &mut rendered);
                out.push_str(&format!("<{tag}>{rendered}</{tag}>"));
                pos = end;
                continue;
            }
        }
        out.push_str(&escape_html(&chars[pos].to_string()));
        pos += 1;
    }
    pos
}

/// Parse one inline link or image at `start`, returning the consumed range and
/// rendered HTML.
fn parse_inline_link(
    chars: &[char],
    start: usize,
    image: bool,
    depth: usize,
) -> Option<(usize, String)> {
    let offset = if image { start + 1 } else { start };
    if offset >= chars.len() || chars[offset] != '[' {
        return None;
    }
    let mut label_end = offset + 1;
    let mut brackets = 1usize;
    while label_end < chars.len() {
        match chars[label_end] {
            '[' => brackets += 1,
            ']' => {
                brackets -= 1;
                if brackets == 0 {
                    break;
                }
            }
            _ => {}
        }
        label_end += 1;
    }
    if label_end >= chars.len() || chars.get(label_end + 1) != Some(&'(') {
        return None;
    }
    let mut cursor = label_end + 2;
    let mut parens = 1usize;
    while cursor < chars.len() && parens > 0 {
        match chars[cursor] {
            '(' => parens += 1,
            ')' => parens -= 1,
            _ => {}
        }
        if parens > 0 {
            cursor += 1;
        }
    }
    if parens != 0 {
        return None;
    }
    let label: String = chars[offset + 1..label_end].iter().collect();
    let mut target: String = chars[label_end + 2..cursor].iter().collect();
    let mut title = String::new();
    if let Some(quote) = target.find('"')
        && let Some(end_quote) = target[quote + 1..].find('"')
    {
        title = target[quote + 1..quote + 1 + end_quote].to_string();
        target.truncate(quote);
    }
    let Some(href) = sanitize_markdown_url(target.trim()) else {
        return Some((cursor + 1, escape_html(&label)));
    };
    let html = if image {
        let mut tag = format!(
            "<img src=\"{}\" alt=\"{}\"",
            escape_html(&href),
            escape_html(&label),
        );
        if !title.is_empty() {
            tag.push_str(&format!(" title=\"{}\"", escape_html(&title)));
        }
        tag.push('>');
        tag
    } else {
        let mut tag = format!("<a href=\"{}\"", escape_html(&href));
        if !title.is_empty() {
            tag.push_str(&format!(" title=\"{}\"", escape_html(&title)));
        }
        tag.push('>');
        tag.push_str(&inline_markdown(&label, depth + 1));
        tag.push_str("</a>");
        tag
    };
    Some((cursor + 1, html))
}

/// Render inline markdown. Code spans, images and links are extracted first so
/// their contents are never re-parsed as emphasis.
fn inline_markdown(text: &str, depth: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut work = String::new();
    let mut placeholders: Vec<String> = Vec::new();
    let mut pos = 0;
    while pos < chars.len() {
        if chars[pos] == '`' {
            let fence = run_length(&chars, pos);
            if let Some((end, _)) = find_run_of_length(&chars, pos + fence, '`', fence) {
                let mut code: String = chars[pos + fence..end].iter().collect();
                if code.starts_with(' ') && code.ends_with(' ') && code.len() > 1 {
                    code = code[1..code.len() - 1].to_string();
                }
                let html = format!("<code>{}</code>", escape_html(&code));
                work.push_str(&push_placeholder(&mut placeholders, html));
                pos = end + fence;
                continue;
            }
        } else if chars[pos] == '!'
            && depth < 8
            && let Some((end, html)) = parse_inline_link(&chars, pos, true, depth)
        {
            work.push_str(&push_placeholder(&mut placeholders, html));
            pos = end;
            continue;
        } else if chars[pos] == '['
            && depth < 8
            && let Some((end, html)) = parse_inline_link(&chars, pos, false, depth)
        {
            work.push_str(&push_placeholder(&mut placeholders, html));
            pos = end;
            continue;
        }
        work.push(chars[pos]);
        pos += 1;
    }

    let work_chars: Vec<char> = work.chars().collect();
    let mut result = String::new();
    emphasis_pass(&work_chars, 0, 0, &mut result);
    for (index, html) in placeholders.iter().enumerate() {
        result = result.replace(&format!("\u{e000}{index}\u{e001}"), html);
    }
    result
}

fn render_code_block(code: &str, language: Option<&str>) -> String {
    let highlighted = language
        .and_then(|lang| syntax_highlight::highlight_to_html(code, lang).ok())
        .unwrap_or_else(|| escape_html(code));
    format!("<pre><code class=\"hljs\">{highlighted}</code></pre>")
}

fn is_list_item(line: &str) -> Option<(bool, &str)> {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix("- ") {
        return Some((false, rest));
    }
    if let Some(rest) = trimmed.strip_prefix("* ") {
        return Some((false, rest));
    }
    if let Some((digits, rest)) = trimmed.split_once(". ")
        && !digits.is_empty()
        && digits.chars().all(|ch| ch.is_ascii_digit())
    {
        return Some((true, rest));
    }
    None
}

/// Render a Markdown fragment with CJK-friendly emphasis. Output matches the
/// shape of upstream's `safeMarkedParse`: block tags end with a newline.
pub fn markdown_to_html(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = String::with_capacity(text.len() + 32);
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }

        if let Some(fence_rest) = trimmed.strip_prefix("```") {
            let language = fence_rest.trim();
            let mut code = String::new();
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with("```") {
                code.push_str(lines[i]);
                code.push('\n');
                i += 1;
            }
            if i < lines.len() {
                i += 1;
            }
            out.push_str(&render_code_block(
                &code,
                if language.is_empty() {
                    None
                } else {
                    Some(language)
                },
            ));
            out.push('\n');
            continue;
        }

        if let Some(level) =
            (1..=6).find(|level| line.starts_with(&format!("{} ", "#".repeat(*level))))
        {
            out.push_str(&format!(
                "<h{level}>{}</h{level}>\n",
                inline_markdown(&line[level + 1..], 0),
            ));
            i += 1;
            continue;
        }

        if trimmed == "---" || trimmed == "***" || trimmed == "___" {
            out.push_str("<hr>\n");
            i += 1;
            continue;
        }

        if let Some(quote) = trimmed.strip_prefix("> ") {
            let mut quoted = vec![quote.to_string()];
            i += 1;
            while i < lines.len()
                && let Some(next) = lines[i].trim_start().strip_prefix("> ")
            {
                quoted.push(next.to_string());
                i += 1;
            }
            out.push_str("<blockquote><p>");
            out.push_str(&inline_markdown(&quoted.join("\n"), 0).replace('\n', "<br>"));
            out.push_str("</p></blockquote>\n");
            continue;
        }

        if let Some((ordered, first)) = is_list_item(line) {
            let tag = if ordered { "ol" } else { "ul" };
            let mut items = vec![first.to_string()];
            i += 1;
            while i < lines.len()
                && let Some((item_ordered, next)) = is_list_item(lines[i])
                && item_ordered == ordered
            {
                items.push(next.to_string());
                i += 1;
            }
            out.push_str(&format!("<{tag}>"));
            for item in items {
                out.push_str(&format!("<li>{}</li>", inline_markdown(&item, 0)));
            }
            out.push_str(&format!("</{tag}>\n"));
            continue;
        }

        if trimmed.contains('|')
            && i + 1 < lines.len()
            && lines[i + 1].trim().starts_with('|')
            && lines[i + 1]
                .trim()
                .chars()
                .all(|ch| matches!(ch, '|' | '-' | ':' | ' '))
        {
            let cells = |row: &str| -> Vec<String> {
                row.trim()
                    .trim_matches('|')
                    .split('|')
                    .map(|cell| inline_markdown(cell.trim(), 0))
                    .collect()
            };
            let headers = cells(line);
            i += 2;
            out.push_str("<table><thead><tr>");
            for cell in headers {
                out.push_str(&format!("<th>{cell}</th>"));
            }
            out.push_str("</tr></thead><tbody>");
            while i < lines.len() && lines[i].trim().contains('|') {
                out.push_str("<tr>");
                for cell in cells(lines[i]) {
                    out.push_str(&format!("<td>{cell}</td>"));
                }
                out.push_str("</tr>");
                i += 1;
            }
            out.push_str("</tbody></table>\n");
            continue;
        }

        let mut paragraph = vec![trimmed.to_string()];
        i += 1;
        while i < lines.len()
            && !lines[i].trim().is_empty()
            && !lines[i].trim_start().starts_with("```")
            && !lines[i].starts_with('#')
            && lines[i].trim() != "---"
            && is_list_item(lines[i]).is_none()
        {
            paragraph.push(lines[i].trim().to_string());
            i += 1;
        }
        out.push_str("<p>");
        out.push_str(&inline_markdown(&paragraph.join("\n"), 0).replace('\n', "<br>"));
        out.push_str("</p>\n");
    }
    out
}

// ---------------------------------------------------------------------------
// Skill blocks
// ---------------------------------------------------------------------------

/// Structural `<skill>` wrapper injected by skill commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillBlock {
    pub name: String,
    pub location: String,
    pub content: String,
    pub user_message: Option<String>,
}

/// Parse the upstream skill wrapper:
/// `<skill name="..." location="...">\n...\n</skill>\n\nuser message`.
pub fn parse_skill_block(text: &str) -> Option<SkillBlock> {
    let opening_end = text.find("\n")?;
    let opening = &text[..opening_end];
    let name = opening
        .strip_prefix("<skill name=\"")?
        .split_once("\" location=\"")?;
    let location = name.1.strip_suffix('>')?;
    let body_start = opening_end + 1;
    let closing = text[body_start..].find("\n</skill>")? + body_start;
    let content = &text[body_start..closing];
    let after = &text[closing + "\n</skill>".len()..];
    let user_message = after.trim_start_matches(['\r', '\n']).trim().to_string();
    Some(SkillBlock {
        name: name.0.to_string(),
        location: location.to_string(),
        content: content.to_string(),
        user_message: if user_message.is_empty() {
            None
        } else {
            Some(user_message)
        },
    })
}

// ---------------------------------------------------------------------------
// Session data and statistics
// ---------------------------------------------------------------------------

/// A session snapshot, independent of the session manager.
#[derive(Debug, Clone, Default)]
pub struct SessionSnapshot {
    pub header: Value,
    pub entries: Vec<Value>,
    pub leaf_id: Option<String>,
    pub system_prompt: Option<String>,
    pub tools: Vec<Value>,
}

impl SessionSnapshot {
    /// Build a snapshot from JSON. Accepts upstream's
    /// `{ header, entries, leafId, systemPrompt, tools }` object or a bare
    /// array of session entries.
    pub fn from_value(value: Value) -> Result<Self, String> {
        if value.is_array() {
            return Ok(Self {
                entries: value
                    .as_array()
                    .ok_or("expected an array of session entries")?
                    .clone(),
                ..Self::default()
            });
        }
        let object = value
            .as_object()
            .ok_or("session data must be an object or an array")?;
        let entries = match object.get("entries") {
            Some(Value::Array(entries)) => entries.clone(),
            Some(Value::Null) | None => Vec::new(),
            Some(_) => return Err("session entries must be an array".to_string()),
        };
        let leaf_id = object
            .get("leafId")
            .or_else(|| object.get("leaf_id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let system_prompt = object
            .get("systemPrompt")
            .or_else(|| object.get("system_prompt"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let tools = object
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(Self {
            header: object.get("header").cloned().unwrap_or(Value::Null),
            entries,
            leaf_id,
            system_prompt,
            tools,
        })
    }

    /// Parse JSONL text: one header line followed by one JSON object per entry.
    pub fn from_jsonl(text: &str) -> Result<Self, String> {
        let mut header = Value::Null;
        let mut entries = Vec::new();
        for (line_number, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(line)
                .map_err(|error| format!("invalid JSON on line {}: {error}", line_number + 1))?;
            let kind = value.get("type").and_then(Value::as_str);
            if header.is_null() && matches!(kind, Some("session") | Some("header")) {
                header = value;
            } else {
                entries.push(value);
            }
        }
        let leaf_id = entries
            .last()
            .and_then(|entry| entry.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(Self {
            header,
            entries,
            leaf_id,
            ..Self::default()
        })
    }
}

/// Options for [`export_session_html`].
#[derive(Debug, Clone)]
pub struct ExportOptions {
    pub title: String,
    pub theme_name: Option<String>,
    pub include_tree: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            title: "Session Export".to_string(),
            theme_name: None,
            include_tree: true,
        }
    }
}

/// Message and token counts, accumulated exactly like upstream's
/// `computeStats` (including the provider-reported cost total fallback).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stats {
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_results: usize,
    pub custom_messages: usize,
    pub compactions: usize,
    pub branch_summaries: usize,
    pub tool_calls: usize,
    pub tokens_input: f64,
    pub tokens_output: f64,
    pub tokens_cache_read: f64,
    pub tokens_cache_write: f64,
    pub cost_input: f64,
    pub cost_output: f64,
    pub cost_cache_read: f64,
    pub cost_cache_write: f64,
    pub cost_total: f64,
    pub models: IndexSet<String>,
}

fn number(value: &Value, key: &str) -> f64 {
    value.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

/// Compute the header statistics of a session.
pub fn compute_stats(entries: &[Value]) -> Stats {
    let mut stats = Stats::default();
    for entry in entries {
        match entry.get("type").and_then(Value::as_str) {
            Some("message") => {
                let Some(message) = entry.get("message") else {
                    continue;
                };
                match message.get("role").and_then(Value::as_str) {
                    Some("user") => stats.user_messages += 1,
                    Some("assistant") => {
                        stats.assistant_messages += 1;
                        if let Some(model) = message.get("model").and_then(Value::as_str) {
                            let name = match message.get("provider").and_then(Value::as_str) {
                                Some(provider) => format!("{provider}/{model}"),
                                None => model.to_string(),
                            };
                            stats.models.insert(name);
                        }
                        if let Some(usage) = message.get("usage") {
                            stats.tokens_input += number(usage, "input");
                            stats.tokens_output += number(usage, "output");
                            stats.tokens_cache_read += number(usage, "cacheRead");
                            stats.tokens_cache_write += number(usage, "cacheWrite");
                            if let Some(cost) = usage.get("cost") {
                                let components = number(cost, "input")
                                    + number(cost, "output")
                                    + number(cost, "cacheRead")
                                    + number(cost, "cacheWrite");
                                stats.cost_input += number(cost, "input");
                                stats.cost_output += number(cost, "output");
                                stats.cost_cache_read += number(cost, "cacheRead");
                                stats.cost_cache_write += number(cost, "cacheWrite");
                                stats.cost_total += cost
                                    .get("total")
                                    .and_then(Value::as_f64)
                                    .unwrap_or(components);
                            }
                        }
                        if let Some(content) = message.get("content").and_then(Value::as_array) {
                            stats.tool_calls += content
                                .iter()
                                .filter(|block| {
                                    block.get("type").and_then(Value::as_str) == Some("toolCall")
                                })
                                .count();
                        }
                    }
                    Some("toolResult") => stats.tool_results += 1,
                    _ => {}
                }
            }
            Some("compaction") => stats.compactions += 1,
            Some("branch_summary") => stats.branch_summaries += 1,
            Some("custom_message") => stats.custom_messages += 1,
            _ => {}
        }
    }
    stats
}

/// Upstream `formatTokens`: 1500 -> `1.5k`, 12000 -> `12k`, 1.5M -> `1.5M`.
pub fn format_tokens(count: f64) -> String {
    if count < 1000.0 {
        format!("{}", count as i64)
    } else if count < 10_000.0 {
        format!("{:.1}k", count / 1000.0)
    } else if count < 1_000_000.0 {
        format!("{}k", (count / 1000.0).round() as i64)
    } else {
        format!("{:.1}M", count / 1_000_000.0)
    }
}

fn format_timestamp(timestamp: &Value) -> String {
    let Some(text) = timestamp.as_str() else {
        return String::new();
    };
    match text.split_once('T') {
        Some((_, time)) => time.chars().take(8).collect(),
        None => text.chars().take(8).collect(),
    }
}

// ---------------------------------------------------------------------------
// Transcript rendering
// ---------------------------------------------------------------------------

struct RenderContext<'a> {
    by_id: HashMap<&'a str, &'a Value>,
    tool_results: HashMap<String, &'a Value>,
    entries: &'a [Value],
}

impl<'a> RenderContext<'a> {
    fn new(entries: &'a [Value]) -> Self {
        let mut by_id = HashMap::new();
        let mut tool_results = HashMap::new();
        for entry in entries {
            if let Some(id) = entry.get("id").and_then(Value::as_str) {
                by_id.insert(id, entry);
            }
            if entry.get("type").and_then(Value::as_str) == Some("message")
                && let Some(message) = entry.get("message")
                && message.get("role").and_then(Value::as_str) == Some("toolResult")
                && let Some(id) = message.get("toolCallId").and_then(Value::as_str)
            {
                tool_results.insert(id.to_string(), message);
            }
        }
        Self {
            by_id,
            tool_results,
            entries,
        }
    }

    fn path_to_leaf(&self, leaf_id: Option<&str>) -> Vec<&'a Value> {
        let mut current = leaf_id
            .and_then(|id| self.by_id.get(id).copied())
            .or_else(|| self.entries.last());
        let mut seen: HashSet<String> = HashSet::new();
        let mut path = Vec::new();
        while let Some(entry) = current {
            let id = entry.get("id").and_then(Value::as_str).unwrap_or("");
            if !seen.insert(id.to_string()) {
                break;
            }
            path.push(entry);
            current = entry
                .get("parentId")
                .and_then(Value::as_str)
                .and_then(|parent| self.by_id.get(parent).copied())
                .filter(|parent| parent.get("id").and_then(Value::as_str) != Some(id));
        }
        path.reverse();
        path
    }

    fn is_leading_system_entry(&self, entry: &Value) -> bool {
        let mut current = entry
            .get("parentId")
            .and_then(Value::as_str)
            .and_then(|id| self.by_id.get(id).copied());
        let mut seen: HashSet<String> = HashSet::new();
        while let Some(ancestor) = current {
            let id = ancestor.get("id").and_then(Value::as_str).unwrap_or("");
            if !seen.insert(id.to_string()) {
                break;
            }
            if carries_system_state(ancestor) {
                return false;
            }
            current = ancestor
                .get("parentId")
                .and_then(Value::as_str)
                .and_then(|parent| self.by_id.get(parent).copied())
                .filter(|parent| parent.get("id").and_then(Value::as_str) != Some(id));
        }
        true
    }
}

fn carries_system_state(entry: &Value) -> bool {
    if entry.get("type").and_then(Value::as_str) == Some("compaction") {
        return entry
            .get("systemMessage")
            .is_some_and(|value| !value.is_null());
    }
    entry.get("type").and_then(Value::as_str) == Some("message")
        && entry
            .get("message")
            .and_then(|message| message.get("role"))
            .and_then(Value::as_str)
            == Some("system")
}

fn content_text(content: Option<&Value>, separator: &str) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(separator),
        _ => String::new(),
    }
}

fn content_images(content: Option<&Value>) -> Vec<&Value> {
    content
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("image"))
                .collect()
        })
        .unwrap_or_default()
}

fn render_images(images: &[&Value], class: &str) -> String {
    if images.is_empty() {
        return String::new();
    }
    let mut html = "<div class=\"message-images\">".to_string();
    for image in images {
        let mime = image
            .get("mimeType")
            .and_then(Value::as_str)
            .unwrap_or("image/png");
        let data = image.get("data").and_then(Value::as_str).unwrap_or("");
        html.push_str(&format!(
            "<img src=\"data:{};base64,{}\" class=\"{}\">",
            escape_html(mime),
            escape_html(data),
            class,
        ));
    }
    html.push_str("</div>");
    html
}

fn normalize(text: &str) -> String {
    text.replace(['\n', '\t'], " ").trim().to_string()
}

fn truncate(text: &str, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        text.to_string()
    } else {
        let shortened: String = chars[..max_chars].iter().collect();
        format!("{shortened}...")
    }
}

fn str_or_empty(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn shorten_path(path: &str) -> String {
    for prefix in ["/Users/", "/home/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            let components: Vec<&str> = rest.split('/').collect();
            if components.len() > 1 {
                return format!("~/{}", components[1..].join("/"));
            }
        }
    }
    path.to_string()
}

/// HTML for a sidebar tree node. Kept here so `template.rs` stays a document
/// skeleton while sharing the exact display rules with the rendered messages.
pub(crate) fn tree_node_display(entry: &Value, label: Option<&str>) -> String {
    let label_html = label
        .map(|label| {
            format!(
                "<span class=\"tree-label\">[{}]</span> ",
                escape_html(label)
            )
        })
        .unwrap_or_default();
    match entry.get("type").and_then(Value::as_str) {
        Some("message") => {
            let Some(message) = entry.get("message") else {
                return label_html;
            };
            let role = message.get("role").and_then(Value::as_str).unwrap_or("");
            match role {
                "user" => {
                    let raw = content_text(message.get("content"), "");
                    if let Some(skill) = parse_skill_block(&raw) {
                        let mut html = format!(
                            "{label_html}<span class=\"tree-role-skill\">skill:</span> {}",
                            escape_html(&skill.name),
                        );
                        if let Some(user) = skill.user_message {
                            html.push_str(&format!(
                                " · <span class=\"tree-role-user\">user:</span> {}",
                                escape_html(&truncate(&normalize(&user), 100)),
                            ));
                        }
                        html
                    } else {
                        format!(
                            "{label_html}<span class=\"tree-role-user\">user:</span> {}",
                            escape_html(&truncate(&normalize(&raw), 100)),
                        )
                    }
                }
                "assistant" => {
                    let text = truncate(&normalize(&content_text(message.get("content"), "")), 100);
                    if !text.is_empty() {
                        format!(
                            "{label_html}<span class=\"tree-role-assistant\">assistant:</span> {}",
                            escape_html(&text),
                        )
                    } else {
                        format!(
                            "{label_html}<span class=\"tree-role-assistant\">assistant:</span> <span class=\"tree-muted\">(no text)</span>",
                        )
                    }
                }
                "toolResult" => {
                    let name = message
                        .get("toolName")
                        .and_then(Value::as_str)
                        .unwrap_or("tool");
                    format!(
                        "{label_html}<span class=\"tree-role-tool\">[{}]</span>",
                        escape_html(name),
                    )
                }
                "bashExecution" => format!(
                    "{label_html}<span class=\"tree-role-tool\">[bash]:</span> {}",
                    escape_html(&truncate(
                        &normalize(&str_or_empty(message, "command")),
                        100
                    )),
                ),
                "system" => format!("{label_html}<span class=\"tree-muted\">[system]</span>"),
                other => format!(
                    "{label_html}<span class=\"tree-muted\">[{}]</span>",
                    escape_html(other),
                ),
            }
        }
        Some("compaction") => format!(
            "{label_html}<span class=\"tree-compaction\">[compaction: {}k tokens]</span>",
            (number(entry, "tokensBefore") / 1000.0).round() as i64,
        ),
        Some("branch_summary") => format!(
            "{label_html}<span class=\"tree-branch-summary\">[branch summary]:</span> {}",
            escape_html(&truncate(&normalize(&str_or_empty(entry, "summary")), 100)),
        ),
        Some("custom_message") => format!(
            "{label_html}<span class=\"tree-custom-message\">[{}]:</span> {}",
            escape_html(&str_or_empty(entry, "customType")),
            escape_html(&truncate(
                &normalize(&content_text(entry.get("content"), "")),
                100
            )),
        ),
        Some("model_change") => format!(
            "{label_html}<span class=\"tree-muted\">[model: {}]</span>",
            escape_html(&str_or_empty(entry, "modelId")),
        ),
        Some("thinking_level_change") => format!(
            "{label_html}<span class=\"tree-muted\">[thinking: {}]</span>",
            escape_html(&str_or_empty(entry, "thinkingLevel")),
        ),
        Some("context_edit") => format!(
            "{label_html}<span class=\"tree-muted\">[context {}]</span>",
            escape_html(&str_or_empty(entry, "targetId")),
        ),
        Some(other) => format!(
            "{label_html}<span class=\"tree-muted\">[{}]</span>",
            escape_html(other),
        ),
        None => label_html,
    }
}

fn get_system_message_text(message: &Value) -> String {
    let mut parts = vec![content_text(message.get("content"), "\n")];
    if let Some(sections) = message.get("sections").and_then(Value::as_object) {
        for value in sections.values() {
            if let Some(text) = value.as_str() {
                parts.push(text.to_string());
            }
        }
    }
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_system_message_update(message: &Value) -> String {
    let mut parts = Vec::new();
    let text = content_text(message.get("content"), "\n");
    if !text.is_empty() {
        parts.push(text);
    }
    if let Some(sections) = message.get("sections").and_then(Value::as_object) {
        for (name, value) in sections {
            match value.as_str() {
                Some(value) => parts.push(format!(
                    "Updated system prompt section \"{name}\":\n\n{value}"
                )),
                None => parts.push(format!("Removed system prompt section \"{name}\".")),
            }
        }
    }
    parts.join("\n\n")
}

fn describe_system_update(message: &Value) -> String {
    let mut parts = Vec::new();
    if !content_text(message.get("content"), "\n").is_empty() {
        parts.push("instructions".to_string());
    }
    if let Some(sections) = message.get("sections").and_then(Value::as_object) {
        for (name, value) in sections {
            parts.push(if value.is_null() {
                format!("-{name}")
            } else {
                name.clone()
            });
        }
    }
    let mut tool_changes = Vec::new();
    for key in ["toolsAdded", "toolsRemoved"] {
        let sign = if key == "toolsAdded" { '+' } else { '-' };
        if let Some(tools) = message.get(key).and_then(Value::as_array) {
            for tool in tools {
                if let Some(name) = tool.get("name").and_then(Value::as_str) {
                    tool_changes.push(format!("{sign}{name}"));
                }
            }
        }
    }
    if !tool_changes.is_empty() {
        parts.push(format!("tools {}", tool_changes.join(" ")));
    }
    parts.join(", ")
}

fn replace_tabs(text: &str) -> String {
    text.replace('\t', "   ")
}

fn highlight_fragment(code: &str, language: Option<&str>) -> String {
    language
        .and_then(|lang| syntax_highlight::highlight_to_html(code, lang).ok())
        .unwrap_or_else(|| escape_html(code))
}

/// Render expandable tool output. Plain lines are wrapped in divs (with CSS
/// `white-space: pre-wrap`); highlighted blocks use `hljs` classes.
fn format_expandable_output(text: &str, max_lines: usize, language: Option<&str>) -> String {
    let text = replace_tabs(text);
    let lines: Vec<&str> = text.split('\n').collect();
    let preview_lines: Vec<&str> = lines.iter().take(max_lines).copied().collect();
    let remaining = lines.len().saturating_sub(max_lines);
    let hint = if remaining > 0 {
        format!("<div class=\"expand-hint\">... ({remaining} more lines)</div>")
    } else {
        String::new()
    };

    if let Some(language) = language {
        if !syntax_highlight::supports_language(language) {
            return format_expandable_output(text.as_str(), max_lines, None);
        }
        let full = highlight_fragment(&text, Some(language));
        if remaining > 0 {
            let preview = highlight_fragment(&preview_lines.join("\n"), Some(language));
            return format!(
                "<details class=\"tool-output expandable\"><summary class=\"output-preview\">\
                   <pre><code class=\"hljs\">{preview}</code></pre>{hint}</summary>\
                 <div class=\"output-full\"><pre><code class=\"hljs\">{full}</code></pre></div></details>",
            );
        }
        return format!(
            "<div class=\"tool-output\"><pre><code class=\"hljs\">{full}</code></pre></div>"
        );
    }

    let render_lines = |slice: &[&str]| -> String {
        slice
            .iter()
            .map(|line| format!("<div>{}</div>", escape_html(line)))
            .collect()
    };
    if remaining > 0 {
        format!(
            "<details class=\"tool-output expandable\"><summary class=\"output-preview\">{}</summary>\
             <div class=\"output-full\">{}</div></details>",
            render_lines(&preview_lines) + &hint,
            render_lines(&lines),
        )
    } else {
        format!("<div class=\"tool-output\">{}</div>", render_lines(&lines))
    }
}

fn language_from_path(path: &str) -> Option<String> {
    let extension = path.rsplit('.').next()?.to_ascii_lowercase();
    let language = match extension.as_str() {
        "ts" | "tsx" => "typescript",
        "js" | "jsx" => "javascript",
        "py" => "python",
        "rb" => "ruby",
        "rs" => "rust",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "cpp" | "hpp" => "cpp",
        "cs" => "csharp",
        "php" => "php",
        "sh" | "bash" | "zsh" => "bash",
        "sql" => "sql",
        "html" => "html",
        "css" | "scss" => "css",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "xml" => "xml",
        "md" => "markdown",
        "dockerfile" => "dockerfile",
        _ => return None,
    };
    Some(language.to_string())
}

fn read_line_range(args: &Value) -> String {
    let offset = args.get("offset");
    let limit = args.get("limit");
    if offset.is_none() && limit.is_none() {
        return String::new();
    }
    let start = match offset {
        Some(value) => value.as_f64().map(|number| number as i64).or_else(|| {
            value
                .as_str()
                .and_then(|text| text.trim().parse::<i64>().ok())
        }),
        None => Some(1),
    };
    let Some(start) = start else {
        return String::new();
    };
    let Some(limit) = limit else {
        return format!(":{start}");
    };
    let count = limit.as_f64().map(|number| number as i64).or_else(|| {
        limit
            .as_str()
            .and_then(|text| text.trim().parse::<i64>().ok())
    });
    let Some(count) = count else {
        return String::new();
    };
    let end = start + count - 1;
    if end > 0 {
        format!(":{start}-{end}")
    } else {
        format!(":{start}")
    }
}

fn tool_result_text(result: &Value) -> String {
    result
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn render_tool_call(call: &Value, context: &RenderContext<'_>) -> String {
    let id = call.get("id").and_then(Value::as_str).unwrap_or("");
    let name = call.get("name").and_then(Value::as_str).unwrap_or("");
    let args = call.get("arguments").cloned().unwrap_or(Value::Null);
    let result = context.tool_results.get(id).copied();
    let is_error = result
        .and_then(|result| result.get("isError"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let status = match result {
        None => "pending",
        Some(_) if is_error => "error",
        Some(_) => "success",
    };
    let mut html = format!(
        "<div class=\"tool-execution {status}\" id=\"tool-call-{}\">",
        escape_html(id),
    );
    match name {
        "bash" => {
            let command = args.get("command").and_then(Value::as_str).unwrap_or("");
            html.push_str(&format!(
                "<div class=\"tool-command\">$ {}</div>",
                escape_html(if command.is_empty() { "..." } else { command }),
            ));
            if let Some(result) = result {
                let output = tool_result_text(result).trim().to_string();
                if !output.is_empty() {
                    html.push_str(&format_expandable_output(&output, 5, None));
                }
            }
        }
        "read" => {
            let path = args
                .get("file_path")
                .or_else(|| args.get("path"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let range = read_line_range(&args);
            html.push_str(&format!(
                "<div class=\"tool-header\"><span class=\"tool-name\">read</span> <span class=\"tool-path\">{}</span></div>",
                escape_html(&shorten_path(path)),
            ));
            if !range.is_empty() {
                html.push_str(&format!(
                    "<span class=\"line-numbers\">{}</span>",
                    escape_html(&range),
                ));
            }
            if let Some(result) = result {
                if let Some(images) = result.get("content") {
                    html.push_str(&render_images(&content_images(Some(images)), "tool-image"));
                }
                let output = tool_result_text(result);
                if !output.is_empty() {
                    html.push_str(&format_expandable_output(
                        &output,
                        10,
                        language_from_path(path).as_deref(),
                    ));
                }
            }
        }
        "write" => {
            let path = args
                .get("file_path")
                .or_else(|| args.get("path"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let content = args.get("content").and_then(Value::as_str).unwrap_or("");
            let line_count = content.split('\n').count();
            let count_html = if line_count > 10 {
                format!("<span class=\"line-count\">({line_count} lines)</span>")
            } else {
                String::new()
            };
            html.push_str(&format!(
                "<div class=\"tool-header\"><span class=\"tool-name\">write</span> <span class=\"tool-path\">{}</span>{count_html}</div>",
                escape_html(&shorten_path(path)),
            ));
            if !content.is_empty() {
                html.push_str(&format_expandable_output(
                    content,
                    10,
                    language_from_path(path).as_deref(),
                ));
            }
            if let Some(result) = result {
                let output = tool_result_text(result).trim().to_string();
                if !output.is_empty() {
                    html.push_str(&format!(
                        "<div class=\"tool-output\"><div>{}</div></div>",
                        escape_html(&output),
                    ));
                }
            }
        }
        "edit" => {
            let path = args
                .get("file_path")
                .or_else(|| args.get("path"))
                .and_then(Value::as_str)
                .unwrap_or("");
            html.push_str(&format!(
                "<div class=\"tool-header\"><span class=\"tool-name\">edit</span> <span class=\"tool-path\">{}</span></div>",
                escape_html(&shorten_path(path)),
            ));
            let diff = result
                .and_then(|result| result.get("details"))
                .and_then(|details| details.get("diff"))
                .and_then(Value::as_str);
            if let Some(diff) = diff {
                html.push_str("<div class=\"tool-diff\">");
                for line in diff.split('\n') {
                    let class = if line.starts_with('+') {
                        "diff-added"
                    } else if line.starts_with('-') {
                        "diff-removed"
                    } else {
                        "diff-context"
                    };
                    html.push_str(&format!(
                        "<div class=\"{}\">{}</div>",
                        class,
                        escape_html(&replace_tabs(line)),
                    ));
                }
                html.push_str("</div>");
            } else if let Some(result) = result {
                let output = tool_result_text(result).trim().to_string();
                if !output.is_empty() {
                    html.push_str(&format!(
                        "<div class=\"tool-output\"><pre>{}</pre></div>",
                        escape_html(&output),
                    ));
                }
            }
        }
        "ls" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
            let limit = args.get("limit").map(|value| {
                format!(
                    " <span class=\"line-count\">(limit {})</span>",
                    escape_html(&value.to_string())
                )
            });
            html.push_str(&format!(
                "<div class=\"tool-header\"><span class=\"tool-name\">ls</span> <span class=\"tool-path\">{}</span>{}</div>",
                escape_html(&shorten_path(path)),
                limit.unwrap_or_default(),
            ));
            if let Some(result) = result {
                let output = tool_result_text(result).trim().to_string();
                if !output.is_empty() {
                    html.push_str(&format_expandable_output(&output, 20, None));
                }
            }
        }
        _ => {
            html.push_str(&format!(
                "<div class=\"tool-header\"><span class=\"tool-name\">{}</span></div>",
                escape_html(name),
            ));
            let pretty = serde_json::to_string_pretty(&args).unwrap_or_else(|_| args.to_string());
            html.push_str(&format!(
                "<div class=\"tool-output\"><pre>{}</pre></div>",
                escape_html(&pretty),
            ));
            if let Some(result) = result {
                let output = tool_result_text(result);
                if !output.is_empty() {
                    html.push_str(&format_expandable_output(&output, 10, None));
                }
            }
        }
    }

    // Render pre-recorded nested calls from codemode-style tools.
    if let Some(nested) = result
        .and_then(|result| result.get("details"))
        .and_then(|details| details.get("nestedCalls"))
        .and_then(|nested| nested.get("calls"))
        .and_then(Value::as_array)
        && !nested.is_empty()
    {
        let mut lines = vec![format!(
            "Nested calls: {}{}",
            nested.len(),
            if nested
                .first()
                .and_then(|call| call.get("complete"))
                .and_then(Value::as_bool)
                == Some(false)
            {
                " (incomplete record)"
            } else {
                ""
            },
        )];
        for call in nested {
            let icon = match call.get("status").and_then(Value::as_str) {
                Some("ok") => "✓",
                Some("error") => "✗",
                Some("unfinished") => "…",
                _ => "?",
            };
            let call_args = match call.get("arguments") {
                Some(Value::Null) | None => format!(
                    "[arguments omitted, {} bytes]",
                    number(call, "argumentsBytes") as i64,
                ),
                Some(value) => serde_json::to_string(value).unwrap_or_default(),
            };
            let duration = call
                .get("durationMs")
                .and_then(Value::as_f64)
                .map(|value| format!(" {}ms", value as i64))
                .unwrap_or_default();
            let error = call
                .get("error")
                .and_then(Value::as_str)
                .map(|error| format!("\n    {}", error.replace('\n', "\n    ")))
                .unwrap_or_default();
            lines.push(format!(
                "{icon} {} {call_args}{duration}{error}",
                str_or_empty(call, "name"),
            ));
        }
        html.push_str(&format_expandable_output(&lines.join("\n"), 1, None));
    }

    html.push_str("</div>");
    html
}

fn render_entry(entry: &Value, context: &RenderContext<'_>) -> String {
    let id = entry.get("id").and_then(Value::as_str).unwrap_or("");
    let timestamp = format_timestamp(entry.get("timestamp").unwrap_or(&Value::Null));
    let ts_html = if timestamp.is_empty() {
        String::new()
    } else {
        format!("<div class=\"message-timestamp\">{timestamp}</div>")
    };
    let copy_button = format!(
        "<button class=\"copy-link-btn\" data-entry-id=\"{}\" title=\"Copy link to this message\"></button>",
        escape_html(id),
    );

    if entry.get("type").and_then(Value::as_str) == Some("message") {
        let Some(message) = entry.get("message") else {
            return String::new();
        };
        match message.get("role").and_then(Value::as_str) {
            Some("user") => {
                let text = content_text(message.get("content"), "\n");
                let images = content_images(message.get("content"));
                if let Some(skill) = parse_skill_block(&text) {
                    let mut html = format!(
                        "<div class=\"skill-user-entry\" id=\"entry-{}\">{copy_button}{ts_html}",
                        escape_html(id),
                    );
                    html.push_str(&format!(
                        "<details class=\"skill-invocation\">\
                           <summary><span class=\"skill-invocation-label\">[skill] {}</span>\
                           <span class=\"skill-invocation-collapsed\">{} (click to expand)</span></summary>\
                           <div class=\"skill-invocation-content markdown-content\">{}</div>\
                         </details>",
                        escape_html(&skill.name),
                        escape_html(&skill.name),
                        markdown_to_html(&skill.content),
                    ));
                    if skill.user_message.is_some() || !images.is_empty() {
                        html.push_str("<div class=\"user-message\">");
                        html.push_str(&render_images(&images, "message-image"));
                        if let Some(user) = skill.user_message.as_deref() {
                            html.push_str(&format!(
                                "<div class=\"markdown-content\">{}</div>",
                                markdown_to_html(user),
                            ));
                        }
                        html.push_str("</div>");
                    }
                    html.push_str("</div>");
                    return html;
                }
                let mut html = format!(
                    "<div class=\"user-message\" id=\"entry-{}\">{copy_button}{ts_html}",
                    escape_html(id),
                );
                html.push_str(&render_images(&images, "message-image"));
                if !text.trim().is_empty() {
                    html.push_str(&format!(
                        "<div class=\"markdown-content\">{}</div>",
                        markdown_to_html(&text),
                    ));
                }
                html.push_str("</div>");
                html
            }
            Some("assistant") => {
                let mut html = format!(
                    "<div class=\"assistant-message\" id=\"entry-{}\">{copy_button}{ts_html}",
                    escape_html(id),
                );
                if let Some(content) = message.get("content").and_then(Value::as_array) {
                    for block in content {
                        match block.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                                if !text.trim().is_empty() {
                                    html.push_str(&format!(
                                        "<div class=\"assistant-text markdown-content\">{}</div>",
                                        markdown_to_html(text),
                                    ));
                                }
                            }
                            Some("thinking") => {
                                let thinking =
                                    block.get("thinking").and_then(Value::as_str).unwrap_or("");
                                if !thinking.trim().is_empty() {
                                    html.push_str("<details class=\"thinking-block\">");
                                    html.push_str(&format!(
                                        "<summary><div class=\"thinking-collapsed\">Thinking ...</div></summary>\
                                         <div class=\"thinking-text\">{}</div>",
                                        escape_html(thinking),
                                    ));
                                    html.push_str("</details>");
                                }
                            }
                            _ => {}
                        }
                    }
                    for block in content {
                        if block.get("type").and_then(Value::as_str) == Some("toolCall") {
                            html.push_str(&render_tool_call(block, context));
                        }
                    }
                }
                match message.get("stopReason").and_then(Value::as_str) {
                    Some("aborted") => html.push_str("<div class=\"error-text\">Aborted</div>"),
                    Some("error") => html.push_str(&format!(
                        "<div class=\"error-text\">Error: {}</div>",
                        escape_html(
                            message
                                .get("errorMessage")
                                .and_then(Value::as_str)
                                .unwrap_or("Unknown error"),
                        ),
                    )),
                    _ => {}
                }
                html.push_str("</div>");
                html
            }
            Some("bashExecution") => {
                let cancelled = message
                    .get("cancelled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let exit_code = message.get("exitCode").and_then(Value::as_i64);
                let is_error = cancelled || exit_code.is_some_and(|code| code != 0);
                let mut html = format!(
                    "<div class=\"tool-execution {}\" id=\"entry-{}\">{ts_html}",
                    if is_error { "error" } else { "success" },
                    escape_html(id),
                );
                html.push_str(&format!(
                    "<div class=\"tool-command\">$ {}</div>",
                    escape_html(&str_or_empty(message, "command")),
                ));
                let output = str_or_empty(message, "output");
                if !output.is_empty() {
                    html.push_str(&format_expandable_output(&output, 10, None));
                }
                if cancelled {
                    html.push_str("<div style=\"color: var(--warning)\">(cancelled)</div>");
                } else if let Some(code) = exit_code
                    && code != 0
                {
                    html.push_str(&format!(
                        "<div style=\"color: var(--error)\">(exit {code})</div>",
                    ));
                }
                html.push_str("</div>");
                html
            }
            Some("toolResult") => String::new(),
            Some("system") => {
                let mut html = String::new();
                if context.is_leading_system_entry(entry) {
                    let text = get_system_message_text(message);
                    if !text.is_empty() {
                        html.push_str(&template::render_system_prompt_block(
                            "System Prompt",
                            &text,
                        ));
                    }
                    let tools = message
                        .get("toolsAdded")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    html.push_str(&template::render_tools_list("Available Tools", &tools));
                } else {
                    let text = render_system_message_update(message);
                    let changes = describe_system_update(message);
                    if !text.is_empty() {
                        let title = if changes.is_empty() {
                            "System Prompt Update".to_string()
                        } else {
                            format!("System Prompt Update: {changes}")
                        };
                        html.push_str(&template::render_system_prompt_block(&title, &text));
                    }
                    let tools = message
                        .get("toolsAdded")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    html.push_str(&template::render_tools_list("Tools Added", &tools));
                    if let Some(removed) = message.get("toolsRemoved").and_then(Value::as_array)
                        && !removed.is_empty()
                    {
                        html.push_str("<div class=\"tools-list\"><div class=\"tools-header\">Tools Removed</div><div class=\"tools-content\">");
                        for tool in removed {
                            html.push_str(&format!(
                                "<div class=\"tool-item\"><span class=\"tool-item-name\">{}</span></div>",
                                escape_html(&str_or_empty(tool, "name")),
                            ));
                        }
                        html.push_str("</div></div>");
                    }
                }
                if html.is_empty() {
                    String::new()
                } else {
                    format!(
                        "<div class=\"system-entry\" id=\"entry-{}\">{ts_html}{html}</div>",
                        escape_html(id),
                    )
                }
            }
            _ => String::new(),
        }
    } else {
        match entry.get("type").and_then(Value::as_str) {
            Some("model_change") => format!(
                "<div class=\"model-change\" id=\"entry-{}\">{ts_html}Switched to model: <span class=\"model-name\">{}/{}</span></div>",
                escape_html(id),
                escape_html(&str_or_empty(entry, "provider")),
                escape_html(&str_or_empty(entry, "modelId")),
            ),
            Some("compaction") => {
                let tokens = number(entry, "tokensBefore") as i64;
                let display = tokens.to_string();
                let summary = str_or_empty(entry, "summary");
                format!(
                    "<details class=\"compaction\" id=\"entry-{}\">\
                       <summary><div class=\"compaction-label\">[compaction]</div>\
                       <div class=\"compaction-collapsed\">Compacted from {display} tokens</div></summary>\
                       <div class=\"compaction-content\"><strong>Compacted from {display} tokens</strong>\n\n{}</div>\
                     </details>",
                    escape_html(id),
                    escape_html(&summary),
                )
            }
            Some("branch_summary") => format!(
                "<div class=\"branch-summary\" id=\"entry-{}\">{ts_html}\
                   <div class=\"branch-summary-header\">Branch Summary</div>\
                   <div class=\"markdown-content\">{}</div></div>",
                escape_html(id),
                markdown_to_html(&str_or_empty(entry, "summary")),
            ),
            Some("custom_message") => {
                let hidden = entry
                    .get("display")
                    .and_then(Value::as_bool)
                    .is_some_and(|display| !display);
                let content = match entry.get("content") {
                    Some(Value::String(text)) => text.clone(),
                    Some(value) => serde_json::to_string(value).unwrap_or_default(),
                    None => String::new(),
                };
                format!(
                    "<div class=\"hook-message{}\" id=\"entry-{}\">{ts_html}\
                       <div class=\"hook-type\">[{}]{}</div>\
                       <div class=\"markdown-content\">{}</div></div>",
                    if hidden { " hook-message-hidden" } else { "" },
                    escape_html(id),
                    escape_html(&str_or_empty(entry, "customType")),
                    if hidden { " · Hidden in terminal" } else { "" },
                    markdown_to_html(&content),
                )
            }
            _ => String::new(),
        }
    }
}

/// Generate a self-contained HTML document for a session snapshot.
pub fn export_session_html(
    session: &SessionSnapshot,
    options: &ExportOptions,
) -> Result<String, String> {
    let stats = compute_stats(&session.entries);
    let header_id = session
        .header
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let date = session
        .header
        .get("timestamp")
        .map(format_timestamp)
        .unwrap_or_else(|| "unknown".to_string());
    let date = if date.is_empty() {
        "unknown".to_string()
    } else {
        date
    };
    let token_parts = [
        (stats.tokens_input, "↑"),
        (stats.tokens_output, "↓"),
        (stats.tokens_cache_read, "R"),
        (stats.tokens_cache_write, "W"),
    ]
    .into_iter()
    .filter(|(count, _)| *count != 0.0)
    .map(|(count, prefix)| format!("{prefix}{}", format_tokens(count)))
    .collect::<Vec<_>>()
    .join(" ");
    let message_parts = [
        (stats.user_messages, "user"),
        (stats.assistant_messages, "assistant"),
        (stats.tool_results, "tool results"),
        (stats.custom_messages, "custom"),
        (stats.compactions, "compactions"),
        (stats.branch_summaries, "branch summaries"),
    ]
    .into_iter()
    .filter(|(count, _)| *count != 0)
    .map(|(count, label)| format!("{count} {label}"))
    .collect::<Vec<_>>()
    .join(", ");

    let mut extra = String::new();
    if session.entries.iter().all(|entry| {
        entry.get("type").and_then(Value::as_str) != Some("message")
            || entry
                .get("message")
                .and_then(|message| message.get("role"))
                .and_then(Value::as_str)
                != Some("system")
    }) {
        if let Some(prompt) = session.system_prompt.as_deref() {
            extra.push_str(&template::render_system_prompt_block(
                "System Prompt",
                prompt,
            ));
        }
        extra.push_str(&template::render_tools_list(
            "Available Tools",
            &session.tools,
        ));
    }

    let models = stats
        .models
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let messages = if message_parts.is_empty() {
        "0".to_string()
    } else {
        message_parts
    };
    let tokens = if token_parts.is_empty() {
        "0".to_string()
    } else {
        token_parts
    };
    let cost = format!("${:.3}", stats.cost_total);
    let header_fields = template::HeaderFields {
        session_id: header_id,
        date: &date,
        models: &models,
        messages: &messages,
        tool_calls: stats.tool_calls,
        tokens: &tokens,
        cost: &cost,
        extra: &extra,
    };
    let header_html = template::render_header_html(&header_fields);

    let context = RenderContext::new(&session.entries);
    let mut messages_html = String::with_capacity(session.entries.len() * 256);
    for entry in context.path_to_leaf(session.leaf_id.as_deref()) {
        messages_html.push_str(&render_entry(entry, &context));
    }
    let tree_html = if options.include_tree {
        template::render_tree_html(&session.entries, session.leaf_id.as_deref())
    } else {
        String::new()
    };

    Ok(template::document_html(
        &styles::document_css(options.theme_name.as_deref()),
        &options.title,
        &header_html,
        &tree_html,
        &messages_html,
        &template::render_footer_html(""),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(entries: Vec<Value>) -> SessionSnapshot {
        SessionSnapshot {
            header: serde_json::json!({"id": "test-session", "timestamp": "2026-01-01T10:20:30.000Z"}),
            entries,
            leaf_id: None,
            ..SessionSnapshot::default()
        }
    }

    fn user_entry(id: &str, content: Value) -> Value {
        serde_json::json!({
            "type": "message",
            "id": id,
            "parentId": Value::Null,
            "timestamp": "2026-01-01T10:20:30.000Z",
            "message": {"role": "user", "content": content},
        })
    }

    #[test]
    fn escaping_uses_the_correct_order() {
        assert_eq!(escape_html("&<>'\""), "&amp;&lt;&gt;&#39;&quot;");
        assert!(!escape_html("&<").contains("&amp;lt;"));
        assert_eq!(escape_html_ansi("'"), "&#039;");
    }

    #[test]
    fn ansi_sgr_converts_to_inline_styles() {
        assert_eq!(
            ansi_to_html("\u{1b}[1;31mred\u{1b}[0m plain"),
            "<span style=\"color:#800000;font-weight:bold\">red</span> plain",
        );
        assert_eq!(
            ansi_to_html("\u{1b}[38;5;196mx"),
            "<span style=\"color:#ff0000\">x</span>",
        );
        assert_eq!(
            ansi_to_html("\u{1b}[48;2;1;2;3mx"),
            "<span style=\"background-color:rgb(1,2,3)\">x</span>",
        );
    }

    #[test]
    fn ansi_osc8_links_match_upstream() {
        let link =
            |url: &str, text: &str, end: &str| format!("\u{1b}]8;;{url}{end}{text}\u{1b}]8;;{end}");
        assert_eq!(
            ansi_to_html(&format!(
                "see {}.",
                link(
                    "https://example.com/a?b=1&c=2",
                    "\u{1b}[4mdocs\u{1b}[24m",
                    "\u{1b}\\",
                ),
            )),
            "see <a href=\"https://example.com/a?b=1&amp;c=2\"><span style=\"text-decoration:underline\">docs</span></a>.",
        );
        assert_eq!(
            ansi_to_html(&link("mailto:me@example.com", "mail", "\u{7}")),
            "<a href=\"mailto:me@example.com\">mail</a>",
        );
        assert_eq!(
            ansi_to_html("\u{1b}[31mred \u{1b}]8;;https://example.com\u{1b}\\link"),
            "<span style=\"color:#800000\">red </span><a href=\"https://example.com\"><span style=\"color:#800000\">link</span></a>",
        );
    }

    #[test]
    fn ansi_drops_unsafe_links_and_unknown_sequences() {
        assert_eq!(ansi_to_html("\u{1b}]8;;javascript:alert(1)\u{1b}\\x"), "x",);
        assert_eq!(
            ansi_to_html("\u{1b}]8;;java\u{1}script:alert(1)\u{1b}\\x"),
            "x",
        );
        assert_eq!(
            strip_ansi_escapes(
                "\u{1b}]0;title\u{7}a\u{1b}[2Kb\u{1b}]133;A\u{1b}\\c\u{1b}_pi:c\u{7}d\u{1b}e"
            ),
            "abcde",
        );
        assert_eq!(
            ansi_lines_to_html(&["one".to_string(), String::new()]),
            "<div class=\"ansi-line\">one</div><div class=\"ansi-line\">&nbsp;</div>",
        );
    }

    #[test]
    fn markdown_cjk_emphasis_flanking() {
        assert_eq!(
            markdown_to_html("**这是测试。**后面"),
            "<p><strong>这是测试。</strong>后面</p>\n",
        );
        assert_eq!(
            markdown_to_html("字**「引用」**字"),
            "<p>字<strong>「引用」</strong>字</p>\n",
        );
        assert_eq!(
            markdown_to_html("*斜体。*后面"),
            "<p><em>斜体。</em>后面</p>\n",
        );
        assert_eq!(
            markdown_to_html("**end.**Then next"),
            "<p>**end.**Then next</p>\n",
        );
    }

    #[test]
    fn markdown_sanitizes_link_schemes_and_escapes_href() {
        let unsafe_html = markdown_to_html("[click](javascript:alert(1))");
        assert!(!unsafe_html.contains("<a"));
        assert!(unsafe_html.contains("click"));
        let safe = markdown_to_html("[click](https://example.com?a=1&b=2)");
        assert!(safe.contains("href=\"https://example.com?a=1&amp;b=2\""));
    }

    #[test]
    fn markdown_highlights_fenced_code() {
        let html = markdown_to_html("```rust\nfn main() {}\n```");
        assert!(html.contains("<pre><code class=\"hljs\">"));
        assert!(html.contains("hljs-"));
        assert!(!html.contains("<script"));
    }

    #[test]
    fn markdown_preserves_paragraph_line_breaks() {
        assert_eq!(markdown_to_html("a\nb"), "<p>a<br>b</p>\n");
    }

    #[test]
    fn skill_block_is_rendered_as_two_sibling_blocks() {
        let text = "<skill name=\"review\" location=\"/skills/review\">\nDo **review**.\n</skill>\n\nPlease review.";
        let html = export_session_html(
            &snapshot(vec![user_entry("s1", Value::String(text.to_string()))]),
            &ExportOptions::default(),
        )
        .expect("export should succeed");
        assert!(html.contains("class=\"skill-user-entry\""));
        assert!(html.contains("[skill] review"));
        assert!(html.contains("<strong>review</strong>"));
        assert!(html.contains("Please review."));
        assert!(!html.contains("<skill name="));
    }

    #[test]
    fn cost_summary_prefers_recorded_totals() {
        let entries = vec![
            serde_json::json!({
                "type": "message",
                "id": "a1",
                "message": {
                    "role": "assistant",
                    "content": [],
                    "model": "gpt-x",
                    "provider": "openai",
                    "usage": {"input": 1500, "output": 20000, "cacheRead": 3, "cost": {
                        "input": 0.1, "output": 0.2, "total": 0.9,
                    }},
                },
            }),
            serde_json::json!({
                "type": "message",
                "id": "a2",
                "message": {
                    "role": "assistant",
                    "content": [],
                    "usage": {"cost": {"input": 0.4, "output": 0.1}},
                },
            }),
        ];
        let stats = compute_stats(&entries);
        assert!((stats.cost_total - 1.4).abs() < f64::EPSILON);
        assert_eq!(
            stats.models.iter().next().map(String::as_str),
            Some("openai/gpt-x")
        );
        let html = export_session_html(&snapshot(entries), &ExportOptions::default())
            .expect("export should succeed");
        assert!(html.contains("$1.400"));
        assert!(html.contains("↑1.5k ↓20k"));
    }

    #[test]
    fn session_snapshot_parses_jsonl_and_reports_bad_lines() {
        let jsonl = concat!(
            "{\"type\":\"session\",\"id\":\"s\",\"timestamp\":\"2026-01-01T00:00:00Z\"}\n",
            "{\"type\":\"message\",\"id\":\"e1\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
        );
        let session = SessionSnapshot::from_jsonl(jsonl).expect("valid jsonl");
        assert_eq!(session.entries.len(), 1);
        assert_eq!(session.leaf_id.as_deref(), Some("e1"));
        assert!(SessionSnapshot::from_jsonl("not json\n").is_err());
    }

    #[test]
    fn deep_tree_exports_without_recursion() {
        let depth = 20_000;
        let mut entries = Vec::with_capacity(depth);
        for i in 0..depth {
            let content = if i == depth - 1 {
                Value::String("deep leaf".to_string())
            } else {
                Value::String("x".to_string())
            };
            entries.push(serde_json::json!({
                "type": "message",
                "id": format!("e{i}"),
                "parentId": if i == 0 { Value::Null } else { Value::String(format!("e{}", i - 1)) },
                "message": {"role": "user", "content": content},
            }));
        }
        let mut session = snapshot(entries);
        session.leaf_id = Some(format!("e{}", depth - 1));
        let html = export_session_html(&session, &ExportOptions::default())
            .expect("deep export should succeed");
        assert!(html.contains("deep leaf"));
        assert_eq!(html.matches("class=\"tree-node").count(), depth);
    }

    #[test]
    fn whitespace_in_tool_output_is_preserved() {
        let css = styles::document_css(None);
        assert!(css.contains(".output-preview > div:not(.expand-hint)"));
        assert!(css.contains(".ansi-line { white-space: pre; }"));
        let output = format_expandable_output("  a\tb\nc", 5, None);
        assert!(output.contains("<div>  a   b</div>"));
        assert!(output.contains("<div>c</div>"));
    }

    #[test]
    fn dynamic_ids_and_tool_names_are_escaped() {
        let entry = serde_json::json!({
            "type": "message",
            "id": "x\" onmouseover=\"bad",
            "message": {
                "role": "assistant",
                "content": [{
                    "type": "toolCall",
                    "id": "c\"<",
                    "name": "<script>bad</script>",
                    "arguments": {"command": "echo hi"},
                }],
            },
        });
        let html = export_session_html(&snapshot(vec![entry]), &ExportOptions::default())
            .expect("export should succeed");
        assert!(!html.contains("<script>bad</script>"));
        assert!(html.contains("tool-call-c&quot;&lt;"));
        assert!(html.contains("data-entry-id=\"x&quot; onmouseover=&quot;bad\""));
    }

    #[test]
    fn system_entry_update_is_rendered() {
        let entries = vec![
            serde_json::json!({
                "type": "message", "id": "s1",
                "message": {"role": "system", "content": "You are helpful.",
                    "sections": {"rules": "be concise"}},
            }),
            serde_json::json!({
                "type": "message", "id": "s2", "parentId": "s1",
                "message": {"role": "system", "content": "",
                    "sections": {"rules": null, "project_context": "v2"},
                    "toolsAdded": [{"name": "bash", "description": "Run commands"}],
                    "toolsRemoved": [{"name": "read"}]},
            }),
        ];
        let context = RenderContext::new(&entries);
        assert!(context.is_leading_system_entry(&entries[0]));
        assert!(!context.is_leading_system_entry(&entries[1]));
        assert_eq!(
            describe_system_update(&entries[1]["message"]),
            "-rules, project_context, tools +bash -read",
        );
        let html = export_session_html(&snapshot(entries), &ExportOptions::default())
            .expect("export should succeed");
        assert!(html.contains("You are helpful."));
        assert!(html.contains("Removed system prompt section &quot;rules&quot;."));
        assert!(html.contains("Tools Removed"));
    }

    #[test]
    fn options_control_title_tree_and_theme() {
        let options = ExportOptions {
            title: "Custom <title>".to_string(),
            theme_name: Some("github-light".to_string()),
            include_tree: false,
        };
        let html =
            export_session_html(&snapshot(Vec::new()), &options).expect("export should succeed");
        assert!(html.contains("Custom &lt;title&gt;"));
        assert!(!html.contains("<nav id=\"sidebar\">"));
        assert!(html.contains("--text: #1e1e1e;"));
    }
}
