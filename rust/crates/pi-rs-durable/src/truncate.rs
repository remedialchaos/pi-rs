//! Whole-line truncation by independent UTF-8 byte and line budgets.

pub const DEFAULT_MAX_LINES: usize = 2_000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TruncationLimit {
    Lines,
    Bytes,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TruncationOptions {
    pub max_lines: Option<usize>,
    pub max_bytes: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextTotals {
    pub lines: usize,
    pub bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TruncationResult<'a> {
    pub content: &'a str,
    pub truncated: bool,
    pub truncated_by: Option<TruncationLimit>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    pub last_line_partial: bool,
    pub first_line_exceeds_limit: bool,
    pub max_lines: usize,
    pub max_bytes: usize,
}

pub const fn utf8_byte_length(content: &str) -> usize {
    content.len()
}

pub fn format_size(bytes: usize) -> String {
    let (unit, suffix) = match bytes {
        0..=1023 => return format!("{bytes}B"),
        1024..=1_048_575 => (1024, "KB"),
        _ => (1_048_576, "MB"),
    };
    // Match positive `toFixed(1)` ties instead of Rust's round-to-even formatting.
    let rounded = (bytes as u128 * 10 + unit / 2) / unit;
    format!("{}.{}{suffix}", rounded / 10, rounded % 10)
}

pub fn truncate_head(content: &str, options: TruncationOptions) -> TruncationResult<'_> {
    let lines = content
        .as_bytes()
        .iter()
        .filter(|&&byte| byte == b'\n')
        .count()
        + usize::from(!content.is_empty() && !content.ends_with('\n'));
    truncate_head_of(
        content,
        TextTotals {
            lines,
            bytes: content.len(),
        },
        options,
    )
}

/// The prefix must be the full text, exceed the byte limit plus one, or contain
/// at least the maximum number of line endings. A trailing LF is not a new line.
pub fn truncate_head_of(
    prefix: &str,
    totals: TextTotals,
    options: TruncationOptions,
) -> TruncationResult<'_> {
    let max_lines = options.max_lines.unwrap_or(DEFAULT_MAX_LINES);
    let max_bytes = options.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
    let mut result = TruncationResult {
        content: prefix,
        truncated: totals.lines > max_lines || totals.bytes > max_bytes,
        truncated_by: None,
        total_lines: totals.lines,
        total_bytes: totals.bytes,
        output_lines: totals.lines,
        output_bytes: totals.bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    };
    if !result.truncated {
        return result;
    }

    let mut lines = prefix.split_terminator('\n');
    let first = lines.next();
    result.first_line_exceeds_limit = first.is_some_and(|line| line.len() > max_bytes);
    let mut limit = if result.first_line_exceeds_limit {
        Some(TruncationLimit::Bytes)
    } else {
        None
    };
    let (mut kept, mut end) = (0, 0);
    if limit.is_none() {
        for (index, line) in first.into_iter().chain(lines).take(max_lines).enumerate() {
            let next = end + usize::from(index > 0) + line.len();
            if next > max_bytes {
                limit = Some(TruncationLimit::Bytes);
                break;
            }
            end = next;
            kept += 1;
        }
    }
    result.content = &prefix[..end];
    result.output_lines = kept;
    result.output_bytes = end;
    result.truncated_by = limit.or(Some(if kept < totals.lines {
        TruncationLimit::Lines
    } else {
        TruncationLimit::Bytes
    }));
    result
}
