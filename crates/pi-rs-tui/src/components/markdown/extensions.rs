use std::borrow::Cow;
use std::ops::Range;
use std::sync::LazyLock;

use pulldown_cmark::{Event, Options, Parser, Tag};
use regex::Regex;

use crate::utils::{is_cjk, is_cjk_punctuation, is_whitespace_char};

pub(super) const OPTIONS: Options = Options::ENABLE_TABLES.union(Options::ENABLE_TASKLISTS);

pub(super) enum Extension<'a> {
    Math {
        raw: &'a str,
        body: &'a str,
        display: bool,
        pending: bool,
    },
    Link {
        label: &'a str,
        destination: Cow<'a, str>,
    },
    Emphasis {
        raw: &'a str,
        body: &'a str,
        delimiter: u8,
        count: usize,
    },
    Escape(&'a str),
}

impl<'a> Extension<'a> {
    pub(super) fn raw(&self) -> &'a str {
        match self {
            Self::Math { raw, .. } | Self::Emphasis { raw, .. } => raw,
            Self::Link { label, .. } => label,
            Self::Escape(raw) => raw,
        }
    }
}

pub(super) struct Prepared<'a> {
    pub text: Cow<'a, str>,
    pub extensions: Vec<Extension<'a>>,
    marker: char,
    end: char,
}

impl<'a> Prepared<'a> {
    pub fn parse(text: &'a str, keep_escape: bool) -> Self {
        let private = '\u{e000}'..='\u{f8ff}';
        let mut markers = private.filter(|&ch| !text.contains(ch));
        let marker = markers.next().unwrap_or('\u{f0000}');
        let end = markers.next().unwrap_or('\u{f0001}');
        if !text.contains(['$', '\\', '~', '@', ':', '.', '*', '_']) {
            return Self {
                text: Cow::Borrowed(text),
                extensions: Vec::new(),
                marker,
                end,
            };
        }
        let parser = Parser::new_ext(text, OPTIONS);
        let mut protect: Vec<_> = parser
            .reference_definitions()
            .iter()
            .map(|(_, def)| def.span.clone())
            .collect();
        protect.extend(
            parser
                .into_offset_iter()
                .filter_map(|(event, range)| match event {
                    Event::Code(_)
                    | Event::Html(_)
                    | Event::InlineHtml(_)
                    | Event::Start(Tag::CodeBlock(_) | Tag::Link { .. } | Tag::Image { .. }) => {
                        Some(range)
                    }
                    _ => None,
                }),
        );
        protect.sort_unstable_by_key(|r| (r.start, r.end));
        let mut protected = protect.into_iter().peekable();
        let mut extensions = Vec::new();
        let mut done = 0;
        let mut index = 0;
        let mut out = String::new();
        while index < text.len() {
            while protected.peek().is_some_and(|span| span.end <= index) {
                protected.next();
            }
            if let Some(span) = protected.peek().filter(|span| span.contains(&index)) {
                index = span.end;
                continue;
            }
            let rest = &text[index..];
            let previous = text[..index].chars().next_back();
            let mut extension = None;
            let is_escaped = escaped(text, index);
            if rest.starts_with(['$', '\\']) && (!is_escaped || rest.starts_with("\\[")) {
                extension = math(
                    text,
                    index,
                    protected.peek().map(|span| span.start),
                    is_escaped,
                );
                if extension.is_none()
                    && !is_escaped
                    && rest.starts_with('\\')
                    && keep_escape
                    && rest.as_bytes().get(1).is_some_and(u8::is_ascii_punctuation)
                {
                    extension = Some(Extension::Escape(&rest[..2]));
                }
            }
            if extension.is_none() && rest.starts_with(['*', '_', '~']) && !is_escaped {
                extension = emphasis(
                    rest,
                    previous,
                    protected
                        .peek()
                        .map_or(rest.len(), |span| span.start - index),
                );
            }
            if extension.is_none()
                && previous.is_none_or(|ch| !ch.is_alphanumeric() && !"_-@".contains(ch))
            {
                extension = autolink(rest);
            }
            if let Some(extension) = extension {
                use std::fmt::Write;
                let len = extension.raw().len();
                out.push_str(&text[done..index]);
                if matches!(extension, Extension::Math { display: true, .. })
                    && index > 0
                    && !out.ends_with("\n\n")
                {
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push('\n');
                } else if is_escaped && matches!(extension, Extension::Math { .. }) {
                    out.push('\n');
                }
                let _ = write!(out, "{marker}{}{end}", extensions.len());
                index += len;
                done = index;
                if matches!(extension, Extension::Math { display: true, .. })
                    && !text[index..].starts_with("\n\n")
                {
                    out.push('\n');
                }
                extensions.push(extension);
            } else {
                // INFALLIBLE: index is maintained at a character boundary below text.len().
                index += rest.chars().next().expect("nonempty suffix").len_utf8();
            }
        }
        let text = if extensions.is_empty() {
            Cow::Borrowed(text)
        } else {
            out.push_str(&text[done..]);
            Cow::Owned(out)
        };
        Self {
            text,
            extensions,
            marker,
            end,
        }
    }

    pub fn parts<'s>(
        &'s self,
        mut text: &'s str,
    ) -> impl Iterator<Item = Result<&'s str, &'s Extension<'a>>> + 's {
        std::iter::from_fn(move || {
            if text.is_empty() {
                return None;
            }
            let Some(start) = text.find(self.marker) else {
                return Some(Ok(std::mem::take(&mut text)));
            };
            if start > 0 {
                let (plain, rest) = text.split_at(start);
                text = rest;
                return Some(Ok(plain));
            }
            if let Some((id, rest)) = text[self.marker.len_utf8()..].split_once(self.end)
                && let Some(ext) = id
                    .parse::<usize>()
                    .ok()
                    .and_then(|id| self.extensions.get(id))
            {
                text = rest;
                return Some(Err(ext));
            }
            let (plain, rest) = text.split_at(self.marker.len_utf8());
            text = rest;
            Some(Ok(plain))
        })
    }

    pub fn restore(&self, text: &str) -> String {
        let mut result = String::with_capacity(text.len());
        for part in self.parts(text) {
            result.push_str(part.unwrap_or_else(Extension::raw));
        }
        result
    }
}

fn escaped(text: &str, index: usize) -> bool {
    text.as_bytes()[..index]
        .iter()
        .rev()
        .take_while(|&&b| b == b'\\')
        .count()
        % 2
        != 0
}

fn closing(text: &str, delimiter: &str, offset: usize) -> Option<Range<usize>> {
    text[offset..]
        .match_indices(delimiter)
        .find_map(|(i, value)| {
            let i = i + offset;
            (!escaped(text, i)).then_some(i..i + value.len())
        })
}

fn pending_math(text: &str) -> bool {
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if (ch == '\\' && chars.peek().is_some_and(char::is_ascii_alphabetic))
            || "_^=+*/<>()[\u{005d}|±≤≥≠≈∈→⇒∞∫∑√-".contains(ch)
        {
            return true;
        }
    }
    false
}

fn table_start(text: &str) -> bool {
    let mut lines = text.split('\n');
    let Some(mut prev) = lines.next() else {
        return false;
    };
    for line in lines {
        if (prev.contains('|') || line.contains('|')) && delimiter_row(line) {
            return true;
        }
        prev = line;
    }
    false
}

fn delimiter_row(line: &str) -> bool {
    let line = line.trim();
    !line.is_empty()
        && line.trim_matches('|').split('|').all(|cell| {
            let cell = cell.trim().trim_matches(':');
            !cell.is_empty() && cell.bytes().all(|b| b == b'-')
        })
}

/// Marked accepts one-column tables when only one of the header and delimiter has a pipe.
pub(super) fn normalize_one_column_tables(source: &str) -> Cow<'_, str> {
    if !source.contains('|') {
        return Cow::Borrowed(source);
    }
    let mut insertions = Vec::new();
    let mut start = 0;
    let mut previous = ("", 0);
    for line in source.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        if delimiter_row(body) && !body.trim().trim_matches('|').contains('|') {
            let header = previous.0.trim();
            let single = !header.is_empty() && !header.trim_matches('|').contains('|');
            if single && header.contains('|') != body.contains('|') {
                insertions.push(if header.contains('|') {
                    start + body.len()
                } else {
                    previous.1
                });
            }
        }
        start += line.len();
        previous = (body, start - (line.len() - body.len()));
    }
    if insertions.is_empty() {
        return Cow::Borrowed(source);
    }
    let mut protected = Parser::new_ext(source, OPTIONS)
        .into_offset_iter()
        .filter_map(|(event, range)| {
            matches!(event, Event::Start(Tag::CodeBlock(_) | Tag::HtmlBlock)).then_some(range)
        })
        .peekable();
    let mut out = String::new();
    let mut done = 0;
    for at in insertions {
        while protected.peek().is_some_and(|span| span.end <= at) {
            protected.next();
        }
        if protected.peek().is_some_and(|span| span.contains(&at)) {
            continue;
        }
        out.push_str(&source[done..at]);
        out.push_str(" |");
        done = at;
    }
    if out.is_empty() {
        return Cow::Borrowed(source);
    }
    out.push_str(&source[done..]);
    Cow::Owned(out)
}

fn math(
    text: &str,
    at: usize,
    protected_at: Option<usize>,
    escaped: bool,
) -> Option<Extension<'_>> {
    let rest = &text[at..];
    let (open, close) = if rest.starts_with("$$") {
        ("$$", "$$")
    } else if rest.starts_with("\\(") {
        ("\\(", "\\)")
    } else if rest.starts_with("\\[") {
        ("\\[", "\\]")
    } else if rest.starts_with('$') && !rest[1..].starts_with(is_whitespace_char) {
        ("$", "$")
    } else {
        return None;
    };
    let line_start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let indent = &text[line_start..at];
    let first = open.len();
    let indent = if open == "\\[" {
        indent.trim_end_matches('\\')
    } else {
        indent
    };
    if matches!(open, "$$" | "\\[") && indent.len() <= 3 && indent.bytes().all(|b| b == b' ') {
        let suffix = rest[first..]
            .match_indices(close)
            .find_map(|(start, delimiter)| {
                let start = start + first;
                let end = start + delimiter.len();
                let tail = rest[end..].trim_start_matches([' ', '\t']);
                (tail.starts_with('\n') || tail.is_empty()).then_some(start..end)
            });
        let body = &rest[first..suffix.as_ref().map_or(rest.len(), |last| last.start)];
        if escaped && table_start(body) {
            return Some(Extension::Math {
                raw: open,
                body: "",
                display: false,
                pending: true,
            });
        }
        if !body.trim().is_empty()
            && !table_start(body)
            && (suffix.is_some() || open == "\\[" || pending_math(body))
        {
            let end = suffix.as_ref().map_or(rest.len(), |end| end.end);
            return Some(Extension::Math {
                raw: &rest[..end],
                body: body.trim(),
                display: true,
                pending: suffix.is_none(),
            });
        }
    }
    if escaped {
        return None;
    }
    let limit = rest
        .find('\n')
        .unwrap_or(rest.len())
        .min(protected_at.map_or(rest.len(), |next| next - at));
    if limit < first {
        return None;
    }
    let line = &rest[..limit];
    if let Some(last) = closing(line, close, first) {
        let body = &line[first..last.start];
        if body.is_empty() {
            return None;
        }
        if open == "$" {
            let after = line[last.end..].chars().next();
            let mut var = body.chars();
            let variable = var
                .next()
                .is_some_and(|ch| ch.is_ascii_uppercase() || ch == '_')
                && var
                    .as_str()
                    .trim_end_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
                    .chars()
                    .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_');
            if body.ends_with(is_whitespace_char)
                || body.contains('`')
                || after.is_some_and(|c| c.is_ascii_digit())
                || (variable && after.is_some_and(|c| c.is_ascii_alphabetic() || c == '_'))
            {
                return None;
            }
        }
        return Some(Extension::Math {
            raw: &line[..last.end],
            body,
            display: false,
            pending: false,
        });
    }
    let tail = &line[first..];
    let shell_variable = tail.starts_with('{')
        || tail.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && !tail.contains(['\\', '^', '=']);
    if open.starts_with('\\') || !shell_variable && !tail.contains('$') && pending_math(tail) {
        return Some(Extension::Math {
            raw: line,
            body: tail,
            display: false,
            pending: true,
        });
    }
    None
}

fn emphasis(text: &str, previous: Option<char>, limit: usize) -> Option<Extension<'_>> {
    let delimiter = text.as_bytes()[0];
    let count = text.bytes().take_while(|&b| b == delimiter).count();
    if !(1..=3).contains(&count)
        || delimiter == b'~' && count != 2
        || previous == Some(char::from(delimiter))
        || count >= limit
    {
        return None;
    }
    let text = &text[..limit];
    let marker = &text[..count];
    let next = text[count..].chars().next()?;
    if is_whitespace_char(next) {
        return None;
    }
    let limit = text.find('\n').unwrap_or(text.len());
    for (i, _) in text[count..limit].match_indices(marker) {
        let i = i + count;
        let body = &text[count..i];
        let last = match body.chars().next_back() {
            Some(ch) => ch,
            None => continue,
        };
        if escaped(text, i)
            || last == char::from(delimiter)
            || is_whitespace_char(last)
            || text[i + count..].starts_with(char::from(delimiter))
        {
            continue;
        }
        let after = text[i + count..].chars().next();
        let cjk = is_cjk(body)
            || previous.is_some_and(|c| is_cjk(c.encode_utf8(&mut [0; 4])))
            || after.is_some_and(|c| is_cjk(c.encode_utf8(&mut [0; 4])));
        if delimiter == b'~' || cjk {
            return Some(Extension::Emphasis {
                raw: &text[..i + count],
                body,
                count,
                delimiter,
            });
        }
    }
    None
}

fn autolink(text: &str) -> Option<Extension<'_>> {
    if ["http://", "https://", "www."]
        .into_iter()
        .any(|prefix| text.starts_with(prefix))
    {
        let end = text
            .find(|c: char| is_whitespace_char(c) || is_cjk_punctuation(c) || "<>\"`".contains(c))
            .unwrap_or(text.len());
        let mut label = text[..end].trim_end_matches(['.', ',', ':', ';', '!', '?', '_', '~']);
        while label.ends_with(')')
            && label.bytes().filter(|&b| b == b')').count()
                > label.bytes().filter(|&b| b == b'(').count()
        {
            label = label[..label.len() - 1].trim_end_matches(['.', ',', ':', ';', '!', '?']);
        }
        if label.len() <= 4 || label.ends_with("://") {
            return None;
        }
        let destination = if label.starts_with("www.") {
            Cow::Owned(format!("http://{label}"))
        } else {
            Cow::Borrowed(label)
        };
        return Some(Extension::Link { label, destination });
    }
    static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
        // INFALLIBLE: Fixed GFM bare email form.
        Regex::new(r"^[a-zA-Z0-9._+\-]+@[a-zA-Z0-9\-]+(?:\.[a-zA-Z0-9\-]+)+")
            .expect("valid bare mail regex")
    });
    let end = text
        .find(|c: char| is_whitespace_char(c) || c == '<')
        .unwrap_or(text.len());
    if !text[..end].contains('@') {
        return None;
    }
    let found = EMAIL.find(&text[..end])?;
    let label = found.as_str();
    Some(Extension::Link {
        label,
        destination: Cow::Owned(format!("mailto:{label}")),
    })
}
