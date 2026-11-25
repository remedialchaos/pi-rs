//! Shared truncation utilities ported from `core/tools/truncate.ts`.
//!
//! Two independent limits apply (lines and bytes); whichever is hit first wins.
//! Head truncation never returns a partial line. Tail truncation returns a
//! partial *first* line when the original last line alone exceeds the byte cap.

pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
pub const GREP_MAX_LINE_LENGTH: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TruncatedBy {
    Lines,
    Bytes,
    None,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TruncationResult {
    pub content: String,
    pub truncated: bool,
    pub truncated_by: TruncatedBy,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    pub last_line_partial: bool,
    pub first_line_exceeds_limit: bool,
    pub max_lines: usize,
    pub max_bytes: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TruncationOptions {
    pub max_lines: Option<usize>,
    pub max_bytes: Option<usize>,
}

impl TruncationOptions {
    fn resolved(self) -> (usize, usize) {
        (
            self.max_lines.unwrap_or(DEFAULT_MAX_LINES),
            self.max_bytes.unwrap_or(DEFAULT_MAX_BYTES),
        )
    }
}

/// `String.split("\n")` with a trailing empty element removed; empty input yields no lines.
fn split_lines_for_counting(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn is_blank_line(line: &str) -> bool {
    line.trim().is_empty()
}

/// JS `toFixed(1)` style rounding (half away from zero).
fn to_fixed_1(value: f64) -> String {
    let rounded = (value * 10.0).round() / 10.0;
    format!("{rounded:.1}")
}

pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{}KB", to_fixed_1(bytes as f64 / 1024.0))
    } else {
        format!("{}MB", to_fixed_1(bytes as f64 / (1024.0 * 1024.0)))
    }
}

fn unchanged(
    content: &str,
    total_lines: usize,
    total_bytes: usize,
    max_lines: usize,
    max_bytes: usize,
) -> TruncationResult {
    TruncationResult {
        content: content.to_string(),
        truncated: false,
        truncated_by: TruncatedBy::None,
        total_lines,
        total_bytes,
        output_lines: total_lines,
        output_bytes: total_bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

pub fn truncate_head(content: &str, options: &TruncationOptions) -> TruncationResult {
    let (max_lines, max_bytes) = options.resolved();
    let total_bytes = content.len();
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return unchanged(content, total_lines, total_bytes, max_lines, max_bytes);
    }

    let first_line_bytes = lines.first().map_or(0, |line| line.len());
    if first_line_bytes > max_bytes {
        return TruncationResult {
            content: String::new(),
            truncated: true,
            truncated_by: TruncatedBy::Bytes,
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: true,
            max_lines,
            max_bytes,
        };
    }

    let mut output_lines: Vec<&str> = Vec::new();
    let mut output_bytes_count = 0usize;
    let mut truncated_by = TruncatedBy::Lines;

    for (i, line) in lines.iter().enumerate().take(max_lines) {
        let line_bytes = line.len() + usize::from(i > 0);
        if output_bytes_count + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }
        output_lines.push(line);
        output_bytes_count += line_bytes;
    }

    if output_lines.len() >= max_lines && output_bytes_count <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }

    let output_content = output_lines.join("\n");
    TruncationResult {
        output_bytes: output_content.len(),
        content: output_content,
        truncated: true,
        truncated_by,
        total_lines,
        total_bytes,
        output_lines: output_lines.len(),
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

fn truncate_string_to_bytes_from_end(text: &str, max_bytes: usize) -> String {
    let bytes = text.as_bytes();
    if bytes.len() <= max_bytes {
        return text.to_string();
    }
    let mut start = bytes.len() - max_bytes;
    while start < bytes.len() && (bytes[start] & 0xc0) == 0x80 {
        start += 1;
    }
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

pub fn truncate_tail(content: &str, options: &TruncationOptions) -> TruncationResult {
    let (max_lines, max_bytes) = options.resolved();
    let total_bytes = content.len();
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return unchanged(content, total_lines, total_bytes, max_lines, max_bytes);
    }

    let mut output_lines: Vec<String> = Vec::new();
    let mut output_bytes_count = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    let mut last_line_partial = false;

    let mut i = lines.len();
    while i > 0 && output_lines.len() < max_lines {
        i -= 1;
        let line = lines[i];
        let line_bytes = line.len() + usize::from(!output_lines.is_empty());

        if output_bytes_count + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            let remaining_bytes = max_bytes
                .saturating_sub(output_bytes_count)
                .saturating_sub(usize::from(!output_lines.is_empty()));
            if output_lines.is_empty()
                || (!is_blank_line(line)
                    && output_lines.iter().all(|l| is_blank_line(l))
                    && remaining_bytes > 0)
            {
                let truncated_line = truncate_string_to_bytes_from_end(line, remaining_bytes);
                let added_bytes = line_bytes - line.len() + truncated_line.len();
                output_lines.insert(0, truncated_line);
                output_bytes_count += added_bytes;
                last_line_partial = true;
            }
            break;
        }

        output_lines.insert(0, line.to_string());
        output_bytes_count += line_bytes;
    }

    if output_lines.len() >= max_lines && output_bytes_count <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }

    let output_content = output_lines.join("\n");
    TruncationResult {
        output_bytes: output_content.len(),
        content: output_content,
        truncated: true,
        truncated_by,
        total_lines,
        total_bytes,
        output_lines: output_lines.len(),
        last_line_partial,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

fn is_high_surrogate(code: u16) -> bool {
    (0xd800..=0xdbff).contains(&code)
}

fn is_low_surrogate(code: u16) -> bool {
    (0xdc00..=0xdfff).contains(&code)
}

/// At most the first `max_units` UTF-16 code units, never splitting a surrogate pair.
pub fn slice_head(text: &str, max_units: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut end = max_units.min(units.len());
    if end > 0
        && is_high_surrogate(units[end - 1])
        && end < units.len()
        && is_low_surrogate(units[end])
    {
        end -= 1;
    }
    String::from_utf16_lossy(&units[..end])
}

/// At most the last `max_units` UTF-16 code units, never splitting a surrogate pair.
pub fn slice_tail(text: &str, max_units: usize) -> String {
    if max_units == 0 {
        return String::new();
    }
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut start = units.len().saturating_sub(max_units);
    if start < units.len()
        && is_low_surrogate(units[start])
        && start > 0
        && is_high_surrogate(units[start - 1])
    {
        start += 1;
    }
    String::from_utf16_lossy(&units[start..])
}

/// Truncate a single line to at most `max_chars` UTF-16 units, adding the marker.
pub fn truncate_line(line: &str, max_chars: usize) -> (String, bool) {
    if line.encode_utf16().count() <= max_chars {
        return (line.to_string(), false);
    }
    (
        format!("{}... [truncated]", slice_head(line, max_chars)),
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_boundaries() {
        assert_eq!(format_size(0), "0B");
        assert_eq!(format_size(1023), "1023B");
        assert_eq!(format_size(50 * 1024), "50.0KB");
        assert_eq!(format_size(1024 * 1024), "1.0MB");
    }

    #[test]
    fn split_lines_ignores_trailing_newline() {
        assert!(split_lines_for_counting("").is_empty());
        assert_eq!(split_lines_for_counting("a\n"), vec!["a"]);
        assert_eq!(split_lines_for_counting("a\nb\n"), vec!["a", "b"]);
        assert_eq!(split_lines_for_counting("a\n\nb"), vec!["a", "", "b"]);
    }

    #[test]
    fn truncate_head_unchanged_when_within_limits() {
        let result = truncate_head("a\nb\nc", &TruncationOptions::default());
        assert!(!result.truncated);
        assert_eq!(result.total_lines, 3);
        assert_eq!(result.content, "a\nb\nc");
    }

    #[test]
    fn truncate_head_by_lines() {
        let content = (1..=5)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = truncate_head(
            &content,
            &TruncationOptions {
                max_lines: Some(3),
                max_bytes: Some(1000),
            },
        );
        assert!(result.truncated);
        assert_eq!(result.truncated_by, TruncatedBy::Lines);
        assert_eq!(result.content, "line 1\nline 2\nline 3");
        assert_eq!(result.total_lines, 5);
        assert_eq!(result.output_lines, 3);
    }

    #[test]
    fn truncate_head_by_bytes() {
        let content = "aaaa\nbbbb\ncccc";
        let result = truncate_head(
            content,
            &TruncationOptions {
                max_lines: Some(100),
                max_bytes: Some(9),
            },
        );
        assert_eq!(result.truncated_by, TruncatedBy::Bytes);
        assert_eq!(result.content, "aaaa\nbbbb");
        assert_eq!(result.output_bytes, 9);
    }

    #[test]
    fn truncate_head_first_line_exceeds_limit() {
        let result = truncate_head(
            "toolongline\nnext",
            &TruncationOptions {
                max_lines: Some(100),
                max_bytes: Some(4),
            },
        );
        assert!(result.first_line_exceeds_limit);
        assert_eq!(result.truncated_by, TruncatedBy::Bytes);
        assert_eq!(result.content, "");
        assert_eq!(result.output_lines, 0);
    }

    #[test]
    fn truncate_tail_by_lines() {
        let content = "a\nb\nc\nd\ne";
        let result = truncate_tail(
            content,
            &TruncationOptions {
                max_lines: Some(2),
                max_bytes: Some(1000),
            },
        );
        assert_eq!(result.truncated_by, TruncatedBy::Lines);
        assert_eq!(result.content, "d\ne");
        assert_eq!(result.total_lines, 5);
    }

    #[test]
    fn truncate_tail_partial_line_with_utf8_boundary() {
        let content = format!("a\n{}END", "x".repeat(20));
        let result = truncate_tail(
            &content,
            &TruncationOptions {
                max_lines: Some(100),
                max_bytes: Some(10),
            },
        );
        assert!(result.last_line_partial);
        assert_eq!(result.truncated_by, TruncatedBy::Bytes);
        assert!(result.content.ends_with("END"));
        assert!(result.output_bytes <= 10);

        let with_multibyte = format!("a\n{}€END", "x".repeat(20));
        let result = truncate_tail(
            &with_multibyte,
            &TruncationOptions {
                max_lines: Some(100),
                max_bytes: Some(10),
            },
        );
        assert!(result.content.ends_with("€END"));
    }

    #[test]
    fn slice_head_ascii_and_surrogates() {
        assert_eq!(slice_head("abcdef", 3), "abc");
        assert_eq!(slice_head("abcdef", 0), "");
        assert_eq!(slice_head("abcdef", 100), "abcdef");
        let emoji = "😀"; // one surrogate pair, 2 UTF-16 units
        assert_eq!(slice_head("😀x😀", 1), "");
        assert_eq!(slice_head("😀x😀", 2), emoji);
        assert_eq!(slice_head("😀x😀", 3), format!("{emoji}x"));
    }

    #[test]
    fn slice_tail_ascii_and_surrogates() {
        assert_eq!(slice_tail("abcdef", 3), "def");
        assert_eq!(slice_tail("abcdef", 0), "");
        assert_eq!(slice_tail("abcdef", 100), "abcdef");
        let emoji = "😀";
        // A cut one unit from the end lands on a low surrogate: upstream drops
        // the whole pair, so the 1-unit result is empty.
        assert_eq!(slice_tail("😀x😀", 1), "");
        assert_eq!(slice_tail("😀x😀", 2), emoji.to_string());
        assert_eq!(slice_tail("😀x😀", 3), format!("x{emoji}"));
    }

    #[test]
    fn truncate_line_keeps_surrogate_pairs_intact() {
        let prefix = format!("{}{}", "a".repeat(499), "😀");
        let text = format!("{prefix}tail");
        let (truncated, was_truncated) = truncate_line(&text, 500);
        assert!(was_truncated);
        assert_eq!(truncated, format!("{}... [truncated]", "a".repeat(499)));
        for unit in truncated.encode_utf16() {
            assert!(!((0xdc00..=0xdfff).contains(&unit) || (0xd800..=0xdbff).contains(&unit)));
        }

        let exact = format!("{}{}", "a".repeat(498), "😀");
        let (same, was_truncated) = truncate_line(&exact, 500);
        assert!(!was_truncated);
        assert_eq!(same, exact);
    }
}
