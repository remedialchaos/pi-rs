//! Terminal Markdown renderer using CommonMark block structure and TUI inline extensions.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Parser, Tag};

use crate::latex::{RenderLatexOptions, render_latex};
use crate::terminal_image::{get_capabilities, hyperlink, is_image_line};
use crate::tui::Component;
use crate::utils::{
    apply_background_to_line, strip_terminal_sequences, visible_width, wrap_text_with_ansi,
};

use super::TextStyleFn;

mod extensions;
mod table;
use extensions::{Extension, OPTIONS, Prepared, normalize_one_column_tables};

#[derive(Default)]
pub struct DefaultTextStyle {
    pub color: Option<TextStyleFn>,
    pub bg_color: Option<TextStyleFn>,
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    pub underline: bool,
}

pub type HighlightCode = Rc<dyn Fn(&str, Option<&str>) -> Vec<String>>;
pub type MarkdownTransform = Rc<dyn Fn(&str, usize) -> String>;

pub struct MarkdownTheme {
    pub heading: TextStyleFn,
    pub link: TextStyleFn,
    pub link_url: TextStyleFn,
    pub code: TextStyleFn,
    pub code_block: TextStyleFn,
    pub code_block_border: TextStyleFn,
    pub quote: TextStyleFn,
    pub quote_border: TextStyleFn,
    pub hr: TextStyleFn,
    pub list_bullet: TextStyleFn,
    pub bold: TextStyleFn,
    pub italic: TextStyleFn,
    pub strikethrough: TextStyleFn,
    pub underline: TextStyleFn,
    pub highlight_code: Option<HighlightCode>,
    pub code_block_indent: Option<String>,
}

impl Default for MarkdownTheme {
    fn default() -> Self {
        let plain: TextStyleFn = Rc::new(str::to_owned);
        let style = |open, close| -> TextStyleFn {
            Rc::new(move |text| {
                if text.is_empty() {
                    String::new()
                } else {
                    let restore = format!("{close}{open}");
                    format!("{open}{}{close}", text.replace(close, &restore))
                }
            })
        };
        Self {
            heading: plain.clone(),
            link: plain.clone(),
            link_url: plain.clone(),
            code: plain.clone(),
            code_block: plain.clone(),
            code_block_border: plain.clone(),
            quote: plain.clone(),
            quote_border: plain.clone(),
            hr: plain.clone(),
            list_bullet: plain,
            bold: style("\x1b[1m", "\x1b[22m"),
            italic: style("\x1b[3m", "\x1b[23m"),
            strikethrough: style("\x1b[9m", "\x1b[29m"),
            underline: style("\x1b[4m", "\x1b[24m"),
            highlight_code: None,
            code_block_indent: None,
        }
    }
}

pub struct MarkdownOptions {
    pub preserve_ordered_list_markers: bool,
    pub preserve_backslash_escapes: bool,
    pub transform: Option<MarkdownTransform>,
    pub render_latex: bool,
}

impl Default for MarkdownOptions {
    fn default() -> Self {
        Self {
            preserve_ordered_list_markers: false,
            preserve_backslash_escapes: false,
            transform: None,
            render_latex: true,
        }
    }
}

type CodeKey = (String, String);
type HighlightedCode = HashMap<CodeKey, (Rc<[String]>, bool)>;

pub struct Markdown {
    text: String,
    padding_x: usize,
    padding_y: usize,
    theme: MarkdownTheme,
    default_style: DefaultTextStyle,
    options: MarkdownOptions,
    render_cache: RefCell<Option<(usize, bool, Vec<String>)>>,
    highlighted: RefCell<HighlightedCode>,
}

enum Kind<'a> {
    Tag(Tag<'a>),
    Text(Cow<'a, str>),
    Code(Cow<'a, str>),
    Html(Cow<'a, str>),
    Break,
    Rule,
    Task(bool),
}

struct Node<'a> {
    kind: Kind<'a>,
    raw: Range<usize>,
    children: Vec<Node<'a>>,
}

fn nodes(parser: Parser<'_>) -> Vec<Node<'_>> {
    let mut result = Vec::new();
    let mut stack: Vec<Node<'_>> = Vec::new();
    for (event, raw) in parser.into_offset_iter() {
        let kind = match event {
            Event::Start(tag) => {
                stack.push(Node {
                    kind: Kind::Tag(tag),
                    raw,
                    children: Vec::new(),
                });
                continue;
            }
            Event::End(_) => {
                if let Some(node) = stack.pop() {
                    if let Some(parent) = stack.last_mut() {
                        parent.children.push(node);
                    } else {
                        result.push(node);
                    }
                }
                continue;
            }
            Event::Text(text)
            | Event::InlineMath(text)
            | Event::DisplayMath(text)
            | Event::FootnoteReference(text) => Kind::Text(text.into()),
            Event::Code(code) => Kind::Code(code.into()),
            Event::Html(html) | Event::InlineHtml(html) => Kind::Html(html.into()),
            Event::SoftBreak | Event::HardBreak => Kind::Break,
            Event::Rule => Kind::Rule,
            Event::TaskListMarker(checked) => Kind::Task(checked),
        };
        let node = Node {
            kind,
            raw,
            children: Vec::new(),
        };
        if let Some(parent) = stack.last_mut() {
            parent.children.push(node);
        } else {
            result.push(node);
        }
    }
    result
}

struct InlineStyle<'a> {
    apply: &'a dyn Fn(&str) -> String,
    prefix: String,
}

fn style_prefix(style: &dyn Fn(&str) -> String) -> String {
    let mut prefix = style("\0");
    if let Some(end) = prefix.find('\0') {
        prefix.truncate(end);
    } else {
        prefix.clear();
    }
    prefix
}

fn strip_comments(text: &str) -> Cow<'_, str> {
    let Some(start) = text.find("<!--") else {
        return Cow::Borrowed(text);
    };
    let mut out = text[..start].to_owned();
    let mut remaining = &text[start..];
    while let Some(end) = remaining.find("-->") {
        remaining = &remaining[end + 3..];
        let next = remaining.find("<!--").unwrap_or(remaining.len());
        out.push_str(&remaining[..next]);
        remaining = &remaining[next..];
    }
    Cow::Owned(out)
}

impl Markdown {
    pub fn new(
        text: impl Into<String>,
        padding_x: usize,
        padding_y: usize,
        theme: MarkdownTheme,
        default_style: DefaultTextStyle,
        options: MarkdownOptions,
    ) -> Self {
        Self {
            text: text.into(),
            padding_x,
            padding_y,
            theme,
            default_style,
            options,
            render_cache: RefCell::new(None),
            highlighted: RefCell::new(HashMap::new()),
        }
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        let text = text.into();
        if self.text != text {
            self.text = text;
            self.render_cache.get_mut().take();
        }
    }

    fn default_style(&self, text: &str) -> String {
        let style = &self.default_style;
        let mut text = style
            .color
            .as_ref()
            .map_or_else(|| text.to_owned(), |color| color(text));
        for (enabled, apply) in [
            (style.bold, &self.theme.bold),
            (style.italic, &self.theme.italic),
            (style.strikethrough, &self.theme.strikethrough),
            (style.underline, &self.theme.underline),
        ] {
            if enabled {
                text = apply(&text);
            }
        }
        text
    }

    fn style_hyperlink(&self, text: &str, label: &str, href: &str) -> String {
        let mut styled = (self.theme.link)(&(self.theme.underline)(text));
        if get_capabilities().hyperlinks {
            return hyperlink(&styled, href);
        }
        if href != label && href.strip_prefix("mailto:") != Some(label) {
            styled.push_str(&(self.theme.link_url)(&format!(" ({href})")));
        }
        styled
    }

    fn latex(&self, raw: &str, body: &str, display: bool, pending: bool, width: usize) -> String {
        if !pending
            && self.options.render_latex
            && let Some(text) = render_latex(
                body,
                &RenderLatexOptions {
                    display: Some(display),
                },
            )
            && (!display || text.lines().all(|line| visible_width(line) <= width))
        {
            return text;
        }
        raw.trim().to_owned()
    }

    fn apply_plain(out: &mut String, text: &str, style: &InlineStyle<'_>) {
        for (i, text) in text.split('\n').enumerate() {
            if i > 0 {
                out.push('\n');
            }
            if !text.is_empty() {
                out.push_str(&(style.apply)(text));
            }
        }
    }

    fn inline(&self, nodes: &[Node<'_>], source: &Prepared<'_>, style: &InlineStyle<'_>) -> String {
        let mut out = String::new();
        let mut drop = false;
        let mut previous_space = true;
        for node in nodes {
            let (next, restore) = match &node.kind {
                Kind::Text(text) => {
                    let text = if drop {
                        text.trim_start_matches([' ', '\t'])
                    } else {
                        text
                    };
                    for part in source.parts(text) {
                        match part {
                            Ok(plain) => Self::apply_plain(&mut out, plain, style),
                            Err(Extension::Math {
                                raw,
                                body,
                                display,
                                pending,
                            }) => Self::apply_plain(
                                &mut out,
                                &self.latex(raw, body, *display, *pending, usize::MAX),
                                style,
                            ),
                            Err(Extension::Escape(raw)) => Self::apply_plain(&mut out, raw, style),
                            Err(Extension::Link { label, destination }) => {
                                out.push_str(&self.style_hyperlink(
                                    &(style.apply)(label),
                                    label,
                                    destination,
                                ));
                                out.push_str(&style.prefix);
                            }
                            Err(Extension::Emphasis {
                                body,
                                delimiter,
                                count,
                                ..
                            }) => {
                                let inner =
                                    Prepared::parse(body, self.options.preserve_backslash_escapes);
                                let parsed = self::nodes(Parser::new_ext(&inner.text, OPTIONS));
                                let text = self.inline(&parsed, &inner, style);
                                let text = if *delimiter == b'~' {
                                    (self.theme.strikethrough)(&text)
                                } else {
                                    let text = if *count >= 2 {
                                        (self.theme.bold)(&text)
                                    } else {
                                        text
                                    };
                                    if count % 2 == 1 {
                                        (self.theme.italic)(&text)
                                    } else {
                                        text
                                    }
                                };
                                out.push_str(&text);
                                out.push_str(&style.prefix);
                            }
                        }
                    }
                    drop = false;
                    previous_space = text.ends_with([' ', '\t']);
                    continue;
                }
                Kind::Code(code) => ((self.theme.code)(code), true),
                Kind::Break => {
                    out.push('\n');
                    drop = false;
                    previous_space = true;
                    continue;
                }
                Kind::Html(html) => {
                    let text = strip_comments(html);
                    if text.is_empty() {
                        drop = previous_space;
                        continue;
                    }
                    ((style.apply)(&text), false)
                }
                Kind::Tag(Tag::Emphasis) => (
                    (self.theme.italic)(&self.inline(&node.children, source, style)),
                    true,
                ),
                Kind::Tag(Tag::Strong) => (
                    (self.theme.bold)(&self.inline(&node.children, source, style)),
                    true,
                ),
                Kind::Tag(Tag::Link { dest_url, .. }) => {
                    let text = self.inline(&node.children, source, style);
                    let plain = strip_terminal_sequences(&text);
                    (self.style_hyperlink(&text, &plain, dest_url), true)
                }
                Kind::Tag(_) => (self.inline(&node.children, source, style), false),
                Kind::Task(_) | Kind::Rule => continue,
            };
            drop = false;
            previous_space = next.ends_with([' ', '\t']);
            out.push_str(&next);
            if restore {
                out.push_str(&style.prefix);
            }
        }
        if !style.prefix.is_empty() {
            while out.ends_with(&style.prefix) {
                out.truncate(out.len() - style.prefix.len());
            }
        }
        out
    }

    fn render_blocks(
        &self,
        nodes: &[Node<'_>],
        source: &Prepared<'_>,
        width: usize,
        style: &InlineStyle<'_>,
    ) -> Vec<String> {
        let mut result = Vec::new();
        let mut previous: Option<&Node<'_>> = None;
        for node in nodes {
            let next = self.block(node, source, width, style);
            if next.is_empty() {
                continue;
            }
            if let Some(before) = previous {
                let prefix = &source.text[..node.raw.start];
                let separator = &prefix[prefix.trim_end_matches([' ', '\t', '\r', '\n']).len()..];
                let blank = separator
                    .bytes()
                    .filter(|&byte| byte == b'\n')
                    .take(2)
                    .count()
                    == 2;
                let spacing = blank
                    || !matches!(
                        (&before.kind, &node.kind),
                        (
                            Kind::Tag(Tag::Paragraph | Tag::List(_)),
                            Kind::Tag(Tag::List(_))
                        )
                    ) && !matches!(before.kind, Kind::Tag(Tag::List(_)));
                if spacing && result.last().is_some_and(|line: &String| !line.is_empty()) {
                    result.push(String::new());
                }
            }
            result.extend(next);
            previous = Some(node);
        }
        result
    }

    fn block(
        &self,
        node: &Node<'_>,
        source: &Prepared<'_>,
        width: usize,
        style: &InlineStyle<'_>,
    ) -> Vec<String> {
        match &node.kind {
            Kind::Tag(Tag::Heading { level, .. }) => {
                let level = match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    HeadingLevel::H5 => 5,
                    HeadingLevel::H6 => 6,
                };
                let heading = |text: &str| {
                    let text = if level == 1 {
                        (self.theme.underline)(text)
                    } else {
                        text.to_owned()
                    };
                    (self.theme.heading)(&(self.theme.bold)(&text))
                };
                let inline = InlineStyle {
                    apply: &heading,
                    prefix: style_prefix(&heading),
                };
                let text = self.inline(&node.children, source, &inline);
                vec![if level >= 3 {
                    heading(&format!("{} ", "#".repeat(level))) + &text
                } else {
                    text
                }]
            }
            Kind::Tag(Tag::Paragraph) => {
                if let [child] = node.children.as_slice()
                    && let Kind::Text(text) = &child.kind
                {
                    let mut parts = source.parts(text);
                    if let Some(Err(Extension::Math {
                        raw,
                        body,
                        display: true,
                        pending,
                    })) = parts.next()
                        && parts.next().is_none()
                    {
                        let text = self.latex(raw, body, true, *pending, width);
                        return text
                            .split('\n')
                            .map(|line| self.default_style(line))
                            .collect();
                    }
                }
                vec![self.inline(&node.children, source, style)]
            }
            Kind::Tag(Tag::CodeBlock(kind)) => self.code_block(node, kind, &source.text),
            Kind::Tag(Tag::List(_)) => {
                let mut lines = Vec::new();
                self.list(node, 0, width, source, style, &mut lines);
                lines
            }
            Kind::Tag(Tag::Table(columns)) => self.table(node, columns.len(), width, source, style),
            Kind::Tag(Tag::BlockQuote(_)) => {
                if width < 3 {
                    return self.render_blocks(&node.children, source, width, style);
                }
                let quote = |text: &str| (self.theme.quote)(&(self.theme.italic)(text));
                let quote_style = InlineStyle {
                    apply: &str::to_owned,
                    prefix: style_prefix(&quote),
                };
                let lines = self.render_blocks(&node.children, source, width - 2, &quote_style);
                let border = (self.theme.quote_border)("│ ");
                let mut output = Vec::new();
                for line in lines {
                    let reset = format!("\x1b[0m{}", quote_style.prefix);
                    let line = if quote_style.prefix.is_empty() {
                        line
                    } else {
                        line.replace("\x1b[0m", &reset)
                    };
                    for line in wrap_text_with_ansi(&quote(&line), width - 2) {
                        output.push(format!("{border}{line}"));
                    }
                }
                output
            }
            Kind::Tag(Tag::HtmlBlock) | Kind::Html(_) => {
                let text = strip_comments(&source.text[node.raw.clone()]);
                let text = text.trim();
                if text.is_empty() {
                    Vec::new()
                } else {
                    vec![self.default_style(text)]
                }
            }
            Kind::Rule => vec![(self.theme.hr)(&"─".repeat(width.min(80)))],
            _ => vec![self.inline(std::slice::from_ref(node), source, style)],
        }
    }

    fn code_block(&self, node: &Node<'_>, kind: &CodeBlockKind<'_>, raw: &str) -> Vec<String> {
        let lang = match kind {
            CodeBlockKind::Fenced(lang) if !lang.is_empty() => Some(lang.as_ref()),
            _ => None,
        };
        let mut code = String::new();
        for child in &node.children {
            if let Kind::Text(text) = &child.kind {
                code.push_str(text);
            }
        }
        if code.ends_with('\n') {
            code.pop();
        }
        let raw = raw[node.raw.clone()].trim_end_matches('\n');
        let (first, last) = raw
            .split_once('\n')
            .map_or((raw, ""), |(a, b)| (a, b.rsplit('\n').next().unwrap_or("")));
        if let Some(mark) = first
            .trim_start()
            .as_bytes()
            .first()
            .copied()
            .filter(|&b| b == b'`' || b == b'~')
        {
            let opening = first
                .trim_start()
                .bytes()
                .take_while(|&b| b == mark)
                .count();
            let partial = last.trim_start();
            if !partial.is_empty()
                && partial.len() < opening
                && partial.bytes().all(|b| b == mark)
                && code.ends_with(partial)
            {
                code.truncate(code.len() - partial.len());
                if code.ends_with('\n') {
                    code.pop();
                }
            }
        }
        let mut lines = vec![(self.theme.code_block_border)(&format!(
            "```{}",
            lang.unwrap_or("")
        ))];
        let indent = self.theme.code_block_indent.as_deref().unwrap_or("  ");
        if let Some(highlight) = &self.theme.highlight_code {
            let key = (lang.unwrap_or("").to_owned(), code);
            let mut cache = self.highlighted.borrow_mut();
            let (result, live) = match cache.entry(key) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let lines = highlight(&entry.key().1, lang);
                    entry.insert((lines.into(), true))
                }
            };
            *live = true;
            lines.extend(result.iter().map(|line| format!("{indent}{line}")));
        } else {
            lines.extend(
                code.split('\n')
                    .map(|line| format!("{indent}{}", (self.theme.code_block)(line))),
            );
        }
        lines.push((self.theme.code_block_border)("```"));
        lines
    }

    fn list(
        &self,
        node: &Node<'_>,
        depth: usize,
        width: usize,
        source: &Prepared<'_>,
        style: &InlineStyle<'_>,
        result: &mut Vec<String>,
    ) {
        let Kind::Tag(Tag::List(start)) = node.kind else {
            return;
        };
        let indent = "    ".repeat(depth);
        let loose = node.children.iter().any(|item| {
            item.children
                .iter()
                .any(|c| matches!(c.kind, Kind::Tag(Tag::Paragraph)))
        });
        for (i, item) in node.children.iter().enumerate() {
            let raw = source.text[item.raw.clone()].trim_start();
            let marker = if self.options.preserve_ordered_list_markers {
                raw.split_whitespace().next().filter(|part| {
                    matches!(*part, "+" | "-" | "*")
                        || part
                            .strip_suffix(['.', ')'])
                            .is_some_and(|digits| digits.bytes().all(|b| b.is_ascii_digit()))
                })
            } else {
                None
            };
            let mut marker = marker.map_or_else(
                || start.map_or_else(|| "-".into(), |start| format!("{}.", start + i as u64)),
                str::to_owned,
            );
            marker.push(' ');
            let task = item.children.iter().find_map(|child| {
                if let Kind::Task(checked) = child.kind {
                    Some(checked)
                } else {
                    child.children.iter().find_map(|node| {
                        if let Kind::Task(checked) = node.kind {
                            Some(checked)
                        } else {
                            None
                        }
                    })
                }
            });
            if let Some(checked) = task {
                marker.push_str(if checked { "[x] " } else { "[ ] " });
            }
            let first = format!("{indent}{}", (self.theme.list_bullet)(&marker));
            let prefix = format!("{indent}{}", " ".repeat(visible_width(&marker)));
            let available = width.saturating_sub(visible_width(&first)).max(1);
            let mut rendered = false;
            let mut index = 0;
            let mut prior_paragraph = false;
            while let Some(child) = item.children.get(index) {
                if matches!(child.kind, Kind::Tag(Tag::List(_))) {
                    self.list(child, depth + 1, width, source, style, result);
                    rendered = true;
                    index += 1;
                    continue;
                }
                let is_block = |node: &Node<'_>| {
                    matches!(
                        node.kind,
                        Kind::Tag(
                            Tag::Paragraph
                                | Tag::Heading { .. }
                                | Tag::BlockQuote(_)
                                | Tag::CodeBlock(_)
                                | Tag::List(_)
                                | Tag::Table(_)
                                | Tag::HtmlBlock
                        ) | Kind::Rule
                    )
                };
                let paragraph = matches!(child.kind, Kind::Tag(Tag::Paragraph));
                let lines = if is_block(child) {
                    index += 1;
                    self.block(child, source, available, style)
                } else {
                    let start = index;
                    index += item.children[start..]
                        .iter()
                        .take_while(|c| !is_block(c))
                        .count();
                    vec![self.inline(&item.children[start..index], source, style)]
                };
                if lines.is_empty() {
                    continue;
                }
                if prior_paragraph && paragraph {
                    result.push(String::new());
                }
                for line in lines {
                    for line in wrap_text_with_ansi(&line, available) {
                        result.push(format!("{}{line}", if rendered { &prefix } else { &first }));
                        rendered = true;
                    }
                }
                prior_paragraph = paragraph;
            }
            if !rendered {
                result.push(first);
            }
            if loose && i + 1 < node.children.len() {
                result.push(String::new());
            }
        }
    }
}

impl Component for Markdown {
    fn invalidate(&mut self) {
        self.render_cache.get_mut().take();
        self.highlighted.get_mut().clear();
    }

    fn render(&self, width: usize) -> Vec<String> {
        let links = get_capabilities().hyperlinks;
        if let Some((_, _, lines)) = self
            .render_cache
            .borrow()
            .as_ref()
            .filter(|(w, h, _)| *w == width && *h == links)
        {
            return lines.clone();
        }
        let available = width
            .saturating_sub(self.padding_x.saturating_mul(2))
            .max(1);
        let source = match &self.options.transform {
            Some(transform) => Cow::Owned(transform(&self.text, available)),
            None => Cow::Borrowed(self.text.as_str()),
        };
        if source.trim().is_empty() {
            return Vec::new();
        }
        let source = if source.contains('\t') {
            Cow::Owned(source.replace('\t', "   "))
        } else {
            source
        };
        let source = normalize_one_column_tables(&source);
        let source = Prepared::parse(&source, self.options.preserve_backslash_escapes);
        let parser = Parser::new_ext(&source.text, OPTIONS);
        let definitions = parser
            .reference_definitions()
            .iter()
            .map(|(_, def)| def.span.clone())
            .collect::<Vec<_>>();
        let ast = nodes(parser);
        let apply = |text: &str| self.default_style(text);
        let style = InlineStyle {
            apply: &apply,
            prefix: style_prefix(&apply),
        };
        for (_, live) in self.highlighted.borrow_mut().values_mut() {
            *live = false;
        }
        let mut body = self.render_blocks(&ast, &source, available, &style);
        if let Some(last) = ast.last()
            && definitions.iter().any(|range| range.start >= last.raw.end)
            && source.text[last.raw.end..]
                .trim_start_matches([' ', '\t', '\r'])
                .starts_with('\n')
        {
            body.push(String::new());
        }
        self.highlighted.borrow_mut().retain(|_, (_, live)| *live);
        let pad = |line: &str| {
            let mut out = " ".repeat(self.padding_x);
            out.push_str(line);
            out.extend(std::iter::repeat_n(
                ' ',
                self.padding_x
                    .max(width.saturating_sub(visible_width(&out))),
            ));
            if let Some(bg) = &self.default_style.bg_color {
                apply_background_to_line(&out, width, |line| bg(line))
            } else {
                out
            }
        };
        let mut lines = Vec::new();
        for _ in 0..self.padding_y {
            lines.push(pad(""));
        }
        for line in body {
            if is_image_line(&line) {
                for line in line.split('\n') {
                    if is_image_line(line) {
                        lines.push(line.to_owned());
                    } else {
                        lines.extend(
                            wrap_text_with_ansi(line, available)
                                .into_iter()
                                .map(|part| pad(&part)),
                        );
                    }
                }
            } else {
                lines.extend(
                    wrap_text_with_ansi(&line, available)
                        .into_iter()
                        .map(|part| pad(&part)),
                );
            }
        }
        for _ in 0..self.padding_y {
            lines.push(pad(""));
        }
        if lines.is_empty() {
            lines.push(String::new());
        }
        *self.render_cache.borrow_mut() = Some((width, links, lines.clone()));
        lines
    }
}
