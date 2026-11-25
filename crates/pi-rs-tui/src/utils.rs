//! Terminal cell measurement, ANSI styling, and layout.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

const ESC: u8 = b'\x1b';
const RESET: &str = "\x1b[0m";
const ATTRIBUTES: [&str; 8] = ["1", "2", "3", "4", "5", "7", "8", "9"];

macro_rules! unicode_regex {
    ($name:ident, $pattern:literal) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| {
            // INFALLIBLE: This Unicode regex is a fixed, valid pattern.
            Regex::new($pattern).expect("valid Unicode property pattern")
        });
    };
}

unicode_regex!(MARK, r"\p{Mark}");
unicode_regex!(
    NON_PRINTING,
    r"[\p{Default_Ignorable_Code_Point}\p{Control}\p{Format}\p{Mark}]"
);
unicode_regex!(
    SPACING_MARK,
    r"^(?:[\p{Spacing_Mark}--[\u1734\u302E\u302F]]|[\u065F\u0F7F\u102B\u102C\u1031\u1033-\u1035\u1038\u103A-\u103E])+$"
);
unicode_regex!(
    CJK,
    r"[\p{scx:Han}\p{scx:Hiragana}\p{scx:Katakana}\p{scx:Hangul}\p{scx:Bopomofo}]"
);
unicode_regex!(PUNCTUATION, r"\p{Punctuation}");

pub fn is_whitespace_char(c: char) -> bool {
    (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}'
}

pub fn is_punctuation_char(c: char) -> bool {
    "(){}[]<>.,;:'\"!?+-=*/\\|&%^$#@~`".contains(c)
}

pub fn is_cjk_punctuation(c: char) -> bool {
    "，．：；！？（）［］｛｝“”‘’…—".contains(c) || {
        let mut buf = [0; 4];
        let s = c.encode_utf8(&mut buf);
        CJK.is_match(s) && PUNCTUATION.is_match(s)
    }
}

pub fn is_autocomplete_separator(c: char) -> bool {
    is_whitespace_char(c) || is_cjk_punctuation(c)
}

pub(crate) fn is_cjk(text: &str) -> bool {
    CJK.is_match(text)
}

pub fn grapheme_width(cluster: &str) -> usize {
    if cluster.len() == 1 {
        return match cluster.as_bytes()[0] {
            b' '..=b'~' => 1,
            b'\t' => 3,
            _ => 0,
        };
    }
    if SPACING_MARK.is_match(cluster) {
        return cluster.chars().count();
    }
    if emojis::get(cluster).is_some_and(|emoji| emoji.as_str() == cluster) {
        return 2;
    }
    let mut buf = [0; 4];
    let mut chars = cluster
        .chars()
        .skip_while(|c| NON_PRINTING.is_match(c.encode_utf8(&mut [0; 4])));
    let Some(first) = chars.next() else {
        return 0;
    };
    if ('\u{1f1e6}'..='\u{1f1ff}').contains(&first) {
        return 2;
    }
    let mut width = first.width().unwrap_or(1);
    let mut follows_mark = false;
    for c in chars {
        let s = c.encode_utf8(&mut buf);
        if SPACING_MARK.is_match(s) {
            width += 1;
            follows_mark = false;
        } else if MARK.is_match(s) {
            follows_mark = true;
        } else if !NON_PRINTING.is_match(s) {
            if follows_mark || ('\u{ff00}'..='\u{ffef}').contains(&c) {
                width += c.width().unwrap_or(1);
            } else if matches!(c, '\u{e33}' | '\u{eb3}') {
                width += 1;
            }
            follows_mark = false;
        }
    }
    width
}

/// Byte length of a supported CSI, OSC, or APC at a UTF-8 byte offset.
pub fn ansi_code_length(text: &str, pos: usize) -> usize {
    let Some([ESC, kind, rest @ ..]) = text.as_bytes().get(pos..) else {
        return 0;
    };
    match kind {
        b'[' => rest
            .iter()
            .position(|b| matches!(b, b'm' | b'G' | b'K' | b'H' | b'J'))
            .map_or(0, |end| end + 3),
        b']' | b'_' => {
            for (i, b) in rest.iter().enumerate() {
                if *b == b'\x07' {
                    return i + 3;
                }
                if *b == ESC && rest.get(i + 1) == Some(&b'\\') {
                    return i + 4;
                }
            }
            0
        }
        _ => 0,
    }
}

pub fn extract_ansi_code(text: &str, pos: usize) -> Option<&str> {
    let length = ansi_code_length(text, pos);
    (length != 0).then(|| &text[pos..pos + length])
}

/// A stream of contiguous plain text and terminal codes, borrowed from the input.
fn runs(mut text: &str) -> impl Iterator<Item = (bool, &str)> {
    std::iter::from_fn(move || {
        if text.is_empty() {
            return None;
        }
        if let Some(code) = extract_ansi_code(text, 0) {
            text = &text[code.len()..];
            return Some((true, code));
        }
        let end = text
            .bytes()
            .enumerate()
            .skip(1)
            .find_map(|(i, b)| (b == ESC && ansi_code_length(text, i) != 0).then_some(i))
            .unwrap_or(text.len());
        let (plain, remainder) = text.split_at(end);
        text = remainder;
        Some((false, plain))
    })
}

fn segments(text: &str) -> impl Iterator<Item = (bool, &str)> {
    runs(text).flat_map(|(ansi, run)| {
        ansi.then_some((true, run)).into_iter().chain(
            (if ansi { "" } else { run })
                .graphemes(true)
                .map(|g| (false, g)),
        )
    })
}

/// ANSI codes do not take cells or interrupt grapheme formation.
pub fn visible_width(text: &str) -> usize {
    let mut bytes = text.as_bytes();
    let mut width = 0;
    while let Some((&b, rest)) = bytes.split_first() {
        match b {
            b' '..=b'~' => width += 1,
            b'\t' => width += 3,
            ESC => {
                let length = ansi_code_length(text, text.len() - bytes.len());
                if length == 0 {
                    break;
                }
                bytes = &bytes[length..];
                continue;
            }
            _ => break,
        }
        bytes = rest;
    }
    if bytes.is_empty() {
        return width;
    }
    let clean = without_ansi(text);
    clean.graphemes(true).map(grapheme_width).sum()
}

fn without_ansi(text: &str) -> Cow<'_, str> {
    let Some(start) = text
        .bytes()
        .enumerate()
        .find_map(|(i, b)| (b == ESC && ansi_code_length(text, i) != 0).then_some(i))
    else {
        return Cow::Borrowed(text);
    };
    let mut output = String::with_capacity(text.len());
    output.push_str(&text[..start]);
    for (ansi, run) in runs(&text[start..]) {
        if !ansi {
            output.push_str(run);
        }
    }
    Cow::Owned(output)
}

pub fn strip_terminal_sequences(text: &str) -> String {
    without_ansi(text).into_owned()
}

pub fn replace_control_characters(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut in_run = false;
    for c in text.chars() {
        if c.is_control() {
            if !in_run {
                output.push(' ');
            }
            in_run = true;
        } else {
            output.push(c);
            in_run = false;
        }
    }
    output
}

pub fn normalize_terminal_output(text: &str) -> Cow<'_, str> {
    if !text.contains(['\t', '\u{e33}', '\u{eb3}']) {
        return Cow::Borrowed(text);
    }
    let mut output = String::with_capacity(text.len());
    for (ansi, run) in runs(text) {
        for c in run.chars() {
            match c {
                '\t' if !ansi => output.push_str("   "),
                '\u{e33}' => output.push_str("\u{e4d}\u{e32}"),
                '\u{eb3}' => output.push_str("\u{ecd}\u{eb2}"),
                _ => output.push(c),
            }
        }
    }
    Cow::Owned(output)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphemeCellRange {
    pub start: usize,
    pub end: usize,
}

pub fn get_grapheme_cell_range(line: &str, column: usize) -> Option<GraphemeCellRange> {
    let mut start = 0;
    for (ansi, segment) in segments(line) {
        if !ansi {
            let end = start + grapheme_width(segment);
            if column < end {
                return Some(GraphemeCellRange { start, end });
            }
            start = end;
        }
    }
    None
}

fn osc8(code: &str) -> Option<(&str, &str)> {
    let body = code.strip_prefix("\x1b]8;")?;
    let (body, close) = if let Some(b) = body.strip_suffix('\x07') {
        (b, "\x1b]8;;\x07")
    } else {
        (body.strip_suffix("\x1b\\")?, "\x1b]8;;\x1b\\")
    };
    let (_, url) = body.split_once(';')?;
    Some((url, close))
}

pub fn get_osc8_link_at_column(line: &str, column: usize) -> Option<&str> {
    let mut col = 0;
    let mut link = None;
    for (ansi, segment) in segments(line) {
        if ansi {
            if let Some((url, _)) = osc8(segment)
                && !url.contains('\x1b')
            {
                link = (!url.is_empty()).then_some(url);
            }
        } else {
            col += grapheme_width(segment);
            if column < col {
                return link;
            }
        }
    }
    None
}

/// Tracks the attributes, colors and hyperlink inherited across a line break.
#[derive(Default)]
pub struct AnsiCodeTracker<'a> {
    attributes: u8,
    foreground: Option<&'a str>,
    background: Option<&'a str>,
    link: Option<(&'a str, &'a str)>,
}

impl<'a> AnsiCodeTracker<'a> {
    pub fn process(&mut self, code: &'a str) {
        if let Some((url, close)) = osc8(code) {
            self.link = (!url.is_empty()).then_some((code, close));
            return;
        }
        let Some(params) = code.strip_prefix("\x1b[").and_then(|s| s.strip_suffix('m')) else {
            return;
        };
        if !params.bytes().all(|b| b.is_ascii_digit() || b == b';') {
            return;
        }
        if params.is_empty() {
            self.reset_style();
            return;
        }
        let mut rest = params;
        loop {
            let start = rest;
            let (part, next) = rest.split_once(';').unwrap_or((rest, ""));
            let has_next = part.len() != rest.len();
            rest = next;
            match part.parse::<u16>() {
                Ok(0) => self.reset_style(),
                Ok(n @ (1..=5 | 7..=9)) => {
                    self.attributes |= 1 << if n <= 5 { n - 1 } else { n - 2 }
                }
                Ok(21) => self.attributes &= !1,
                Ok(22) => self.attributes &= !3,
                Ok(n @ (23..=25 | 27..=29)) => {
                    self.attributes &= !(1 << if n <= 25 { n - 21 } else { n - 22 })
                }
                Ok(30..=37 | 90..=97) => self.foreground = Some(part.trim_start_matches('0')),
                Ok(39) => self.foreground = None,
                Ok(40..=47 | 100..=107) => self.background = Some(part.trim_start_matches('0')),
                Ok(49) => self.background = None,
                Ok(n @ (38 | 48)) if has_next => {
                    let mut suffix = rest.splitn(5, ';');
                    let count = match suffix.next() {
                        Some("5") => 1,
                        Some("2") => 3,
                        _ => 0,
                    };
                    if count != 0
                        && let Some(last) = suffix.nth(count - 1)
                    {
                        let end = last.as_ptr() as usize - start.as_ptr() as usize + last.len();
                        let color = Some(&start[..end]);
                        if n == 38 {
                            self.foreground = color;
                        } else {
                            self.background = color;
                        }
                        if let Some(next) = start[end..].strip_prefix(';') {
                            rest = next;
                            continue;
                        }
                        break;
                    }
                }
                _ => {}
            }
            if !has_next {
                break;
            }
        }
    }

    fn reset_style(&mut self) {
        self.attributes = 0;
        self.foreground = None;
        self.background = None;
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    fn track(&mut self, text: &'a str) {
        for (ansi, run) in runs(text) {
            if ansi {
                self.process(run);
            }
        }
    }

    pub fn has_active_codes(&self) -> bool {
        self.attributes != 0
            || self.foreground.is_some()
            || self.background.is_some()
            || self.link.is_some()
    }

    fn append_active_codes(&self, output: &mut String) {
        let start = output.len();
        for code in ATTRIBUTES
            .into_iter()
            .enumerate()
            .filter_map(|(i, code)| (self.attributes & (1 << i) != 0).then_some(code))
            .chain(self.foreground)
            .chain(self.background)
        {
            output.push_str(if output.len() == start { "\x1b[" } else { ";" });
            output.push_str(code);
        }
        if output.len() != start {
            output.push('m');
        }
        if let Some((open, _)) = self.link {
            output.push_str(open);
        }
    }

    pub fn active_codes(&self) -> String {
        let mut output = String::new();
        self.append_active_codes(&mut output);
        output
    }

    fn close_line(&self, output: &mut String) {
        if self.attributes & (1 << 3) != 0 {
            output.push_str("\x1b[24m");
        }
        if self.attributes & (1 << 7) != 0 {
            output.push_str("\x1b[29m");
        }
        if let Some((_, close)) = self.link {
            output.push_str(close);
        }
    }
}

pub fn get_active_background_ansi(text: &str) -> String {
    let mut tracker = AnsiCodeTracker::default();
    tracker.track(text);
    tracker
        .background
        .map_or_else(String::new, |bg| format!("\x1b[{bg}m"))
}

fn wrap_tokens(text: &str) -> Vec<&str> {
    let mut words = Vec::new();
    let mut start = 0;
    let mut end = 0;
    let mut boundary = 0;
    let mut space = None;
    for (ansi, segment) in segments(text) {
        end += segment.len();
        if ansi {
            continue;
        }
        let is_space = segment == " ";
        let cjk = !is_space && !segment.is_ascii() && CJK.is_match(segment);
        if (space != Some(is_space) || cjk) && start < boundary {
            words.push(&text[start..boundary]);
            start = boundary;
        }
        boundary = end;
        if cjk {
            words.push(&text[start..end]);
            start = end;
            space = None;
        } else {
            space = Some(is_space);
        }
    }
    if start != end {
        if start == boundary
            && let Some(last) = words.last_mut()
        {
            *last = &text[start - last.len()..end];
        } else {
            words.push(&text[start..end]);
        }
    }
    words
}

pub fn wrap_text_with_ansi(text: &str, width: usize) -> Vec<String> {
    let mut result = Vec::new();
    let mut styles = AnsiCodeTracker::default();
    let mut lines = text.split_inclusive(['\r', '\n']).peekable();
    while let Some(line) = lines.next() {
        let ended = line.ends_with(['\r', '\n']);
        let input = if ended { &line[..line.len() - 1] } else { line };
        let mut prefixed = styles.active_codes();
        prefixed.push_str(input);
        wrap_single_line(prefixed, width, &mut result);
        styles.track(input);
        if line.ends_with('\r') && lines.peek() == Some(&"\n") {
            lines.next();
        }
        if ended && lines.peek().is_none() {
            result.push(styles.active_codes());
        }
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}

fn trim_end(text: &mut String) {
    text.truncate(text.trim_end_matches(is_whitespace_char).len());
}

fn wrap_single_line(line: String, width: usize, output: &mut Vec<String>) {
    if visible_width(&line) <= width {
        output.push(line);
        return;
    }
    let mut styles = AnsiCodeTracker::default();
    let mut current = String::new();
    let mut cells = 0;
    let initial_lines = output.len();
    for token in wrap_tokens(&line) {
        let token_width = visible_width(token);
        let whitespace = runs(token).all(|(ansi, run)| ansi || run.chars().all(is_whitespace_char));
        if token_width > width && !whitespace {
            if !current.is_empty() {
                trim_end(&mut current);
                styles.close_line(&mut current);
                output.push(std::mem::take(&mut current));
            }
            styles.append_active_codes(&mut current);
            cells = 0;
            for (ansi, part) in segments(token) {
                if ansi {
                    styles.process(part);
                } else {
                    let w = grapheme_width(part);
                    if cells + w > width {
                        styles.close_line(&mut current);
                        trim_end(&mut current);
                        output.push(std::mem::take(&mut current));
                        styles.append_active_codes(&mut current);
                        cells = 0;
                    }
                    cells += w;
                }
                current.push_str(part);
            }
            cells = visible_width(&current);
        } else {
            let total = cells + token_width;
            if total > width && cells != 0 {
                trim_end(&mut current);
                if whitespace {
                    for (ansi, code) in runs(token) {
                        if ansi {
                            current.push_str(code);
                            styles.process(code);
                        }
                    }
                }
                styles.close_line(&mut current);
                output.push(std::mem::take(&mut current));
                styles.append_active_codes(&mut current);
                cells = 0;
                if whitespace {
                    continue;
                }
            }
            if whitespace && total > width && token.as_bytes().contains(&ESC) {
                current.extend(runs(token).filter_map(|(ansi, part)| ansi.then_some(part)));
            } else {
                current.push_str(token);
                cells += token_width;
            }
            styles.track(token);
        }
    }
    if !current.is_empty() || output.len() == initial_lines {
        trim_end(&mut current);
        output.push(current);
    }
}

pub fn apply_background_to_line(
    line: &str,
    width: usize,
    background: impl FnOnce(&str) -> String,
) -> String {
    let padding = width.saturating_sub(visible_width(line));
    if padding == 0 {
        return background(line);
    }
    let mut padded = String::with_capacity(line.len() + padding);
    padded.push_str(line);
    padded.extend(std::iter::repeat_n(' ', padding));
    background(&padded)
}

pub fn truncate_to_width(text: &str, width: usize, keep_ansi: bool) -> String {
    if keep_ansi {
        truncate_with_ellipsis(text, width, "...", false)
    } else {
        let text = without_ansi(text);
        let truncated = truncate_with_ellipsis(&text, width, "...", false);
        strip_terminal_sequences(&truncated)
    }
}

fn cut_prefix(text: &str, width: usize) -> (&str, usize) {
    if width == 0 {
        return ("", 0);
    }
    let mut pos = 0;
    let mut end = 0;
    let mut kept = 0;
    for (ansi, part) in segments(text) {
        pos += part.len();
        if !ansi {
            let w = grapheme_width(part);
            if kept + w > width {
                break;
            }
            kept += w;
            end = pos;
        }
    }
    (&text[..end], kept)
}

/// Upstream truncation including the caller-selectable ellipsis and padding.
pub fn truncate_with_ellipsis(text: &str, width: usize, ellipsis: &str, pad: bool) -> String {
    if width == 0 {
        return String::new();
    }
    let ellipsis_width = visible_width(ellipsis);
    let actual = if ellipsis_width >= width {
        visible_width(text)
    } else {
        segments(text)
            .filter(|(ansi, _)| !ansi)
            .map(|(_, part)| grapheme_width(part))
            .sum()
    };
    let (mut result, used) = if actual <= width {
        (text.to_owned(), actual)
    } else {
        let ((prefix, kept), (ellipsis, suffix)) = if ellipsis_width >= width {
            (("", 0), cut_prefix(ellipsis, width))
        } else {
            (
                cut_prefix(text, width - ellipsis_width),
                (ellipsis, ellipsis_width),
            )
        };
        if ellipsis_width >= width && suffix == 0 {
            (String::new(), 0)
        } else {
            let close = runs(prefix)
                .filter_map(|(ansi, s)| ansi.then(|| osc8(s)).flatten())
                .last()
                .filter(|(url, _)| !url.is_empty())
                .map_or("", |(_, close)| close);
            let mut output = String::with_capacity(
                prefix.len() + close.len() + RESET.len() * 2 + ellipsis.len(),
            );
            output.push_str(prefix);
            output.push_str(close);
            output.push_str(RESET);
            if !ellipsis.is_empty() {
                output.push_str(ellipsis);
                output.push_str(RESET);
            }
            (output, kept + suffix)
        }
    };
    if pad {
        result.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
    }
    result
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ColumnSlice {
    pub text: String,
    pub width: usize,
}

pub fn slice_by_column(line: &str, start_col: usize, length: usize, strict: bool) -> String {
    slice_with_width(line, start_col, length, strict).text
}

pub fn slice_with_width(line: &str, start_col: usize, length: usize, strict: bool) -> ColumnSlice {
    let mut result = ColumnSlice::default();
    if length == 0 {
        return result;
    }
    let end = start_col.saturating_add(length);
    let mut col = 0;
    let mut pending = String::new();
    for (ansi, part) in segments(line) {
        if ansi {
            if col >= start_col {
                result.text.push_str(&pending);
                pending.clear();
                result.text.push_str(part);
            } else if !part.starts_with("\x1b_") {
                pending.push_str(part);
            }
        } else {
            let w = grapheme_width(part);
            if col >= start_col && (!strict || col + w <= end) {
                result.text.push_str(&pending);
                pending.clear();
                result.text.push_str(part);
                result.width += w;
            }
            col += w;
            if col >= end {
                break;
            }
        }
    }
    result
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct OverlaySegments {
    pub before: String,
    pub before_width: usize,
    pub after: String,
    pub after_width: usize,
}

pub fn extract_segments(
    line: &str,
    before_end: usize,
    after_start: usize,
    after_len: usize,
    strict_after: bool,
) -> OverlaySegments {
    let mut result = OverlaySegments::default();
    let mut styles = AnsiCodeTracker::default();
    let mut pending_before = String::new();
    let mut after_started = false;
    let mut col = 0;
    let after_end = after_start.saturating_add(after_len);
    let end = if after_len == 0 {
        before_end
    } else {
        after_end
    };
    for (ansi, part) in segments(line) {
        if ansi {
            styles.process(part);
            if col < before_end {
                pending_before.push_str(part);
            } else if (after_start..after_end).contains(&col)
                && (after_started || part.starts_with("\x1b_"))
            {
                result.after.push_str(part);
            }
            continue;
        }
        let w = grapheme_width(part);
        if col < before_end && col + w <= before_end {
            result.before.push_str(&pending_before);
            pending_before.clear();
            result.before.push_str(part);
            result.before_width += w;
        } else if (after_start..after_end).contains(&col) && (!strict_after || col + w <= after_end)
        {
            if !after_started {
                styles.append_active_codes(&mut result.after);
                after_started = true;
            }
            result.after.push_str(part);
            result.after_width += w;
        }
        col += w;
        if col >= end {
            break;
        }
    }
    result
}
