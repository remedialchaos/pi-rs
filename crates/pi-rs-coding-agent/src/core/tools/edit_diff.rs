//! Diff and edit primitives ported from `core/tools/edit-diff.ts`.
//!
//! All offsets in this module are Rust UTF-8 byte offsets into a `str`. Matches
//! come from `str::find` on valid UTF-8, so every offset is a character boundary.

use std::fs;
use std::path::Path;

use similar::{ChangeTag, TextDiff};
use unicode_normalization::UnicodeNormalization;

use super::path_utils::resolve_to_cwd;

pub const DIFF_WINDOW_MARGIN_LINES: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextReplacement {
    pub match_index: usize,
    pub match_length: usize,
    pub new_text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct Edit {
    #[serde(rename = "oldText", alias = "old_text")]
    pub old_text: String,
    #[serde(rename = "newText", alias = "new_text")]
    pub new_text: String,
}

#[derive(Clone, Debug)]
pub struct FuzzyMatchResult {
    pub found: bool,
    /// -1 when not found; otherwise a byte offset into `content_for_replacement`.
    pub index: isize,
    pub match_length: usize,
    pub used_fuzzy_match: bool,
    pub content_for_replacement: String,
}

#[derive(Clone, Debug)]
pub struct AppliedEditsResult {
    pub base_content: String,
    pub new_content: String,
    pub replacements: Vec<TextReplacement>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditDiff {
    pub diff: String,
    pub patch: String,
    pub first_changed_line: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditDiffResult {
    pub diff: String,
    pub first_changed_line: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditDiffError {
    pub error: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditDiffOutput {
    Ok(EditDiffResult),
    Err(EditDiffError),
}

#[derive(Clone, Debug)]
pub struct DiffStringResult {
    pub diff: String,
    pub first_changed_line: Option<usize>,
}

fn safe_slice(text: &str, start: usize, end: usize) -> &str {
    let start = start.min(text.len());
    let end = end.min(text.len()).max(start);
    text.get(start..end).unwrap_or("")
}

/// `normalizeToLF`: CRLF and lone CR both become LF.
pub fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Split a leading UTF-8 BOM from decoded text (`utils/text.ts:splitBom`).
pub fn split_bom(content: &str) -> (String, String) {
    if let Some(rest) = content.strip_prefix('\u{feff}') {
        ("\u{feff}".to_string(), rest.to_string())
    } else {
        (String::new(), content.to_string())
    }
}

/// NFKC + per-line trailing whitespace strip + smart punctuation normalization.
pub fn normalize_for_fuzzy_match(text: &str) -> String {
    let nfkc: String = text.nfkc().collect();
    let trimmed_lines = nfkc
        .split('\n')
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n");

    trimmed_lines
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => '\'',
            '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => '"',
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{00a0}' | '\u{2002}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

/// Exact match first, then fuzzy-normalized match in normalized space.
pub fn fuzzy_find_text(content: &str, old_text: &str) -> FuzzyMatchResult {
    if let Some(index) = content.find(old_text) {
        return FuzzyMatchResult {
            found: true,
            index: index as isize,
            match_length: old_text.len(),
            used_fuzzy_match: false,
            content_for_replacement: content.to_string(),
        };
    }

    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old_text = normalize_for_fuzzy_match(old_text);
    match fuzzy_content.find(&fuzzy_old_text) {
        Some(index) => FuzzyMatchResult {
            found: true,
            index: index as isize,
            match_length: fuzzy_old_text.len(),
            used_fuzzy_match: true,
            content_for_replacement: fuzzy_content,
        },
        None => FuzzyMatchResult {
            found: false,
            index: -1,
            match_length: 0,
            used_fuzzy_match: false,
            content_for_replacement: content.to_string(),
        },
    }
}

fn count_occurrences(content: &str, old_text: &str) -> usize {
    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old_text = normalize_for_fuzzy_match(old_text);
    if fuzzy_old_text.is_empty() {
        return 0;
    }
    fuzzy_content.matches(&fuzzy_old_text).count()
}

fn get_not_found_error(path: &str, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        format!(
            "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
        )
    } else {
        format!(
            "Could not find edits[{edit_index}] in {path}. The oldText must match exactly including all whitespace and newlines."
        )
    }
}

fn get_duplicate_error(
    path: &str,
    edit_index: usize,
    total_edits: usize,
    occurrences: usize,
) -> String {
    if total_edits == 1 {
        format!(
            "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
        )
    } else {
        format!(
            "Found {occurrences} occurrences of edits[{edit_index}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
        )
    }
}

fn get_empty_old_text_error(path: &str, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        format!("oldText must not be empty in {path}.")
    } else {
        format!("edits[{edit_index}].oldText must not be empty in {path}.")
    }
}

fn get_no_change_error(path: &str, total_edits: usize) -> String {
    if total_edits == 1 {
        format!(
            "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
        )
    } else {
        format!("No changes made to {path}. The replacements produced identical content.")
    }
}

#[derive(Clone, Copy, Debug)]
struct LineSpan {
    start: usize,
    end: usize,
}

/// `[^\n]*\n|[^\n]+` — each span includes its trailing newline, if any.
fn get_line_spans(content: &str) -> Vec<LineSpan> {
    let mut spans = Vec::new();
    let mut offset = 0;
    for (index, byte) in content.bytes().enumerate() {
        if byte == b'\n' {
            let end = index + 1;
            spans.push(LineSpan { start: offset, end });
            offset = end;
        }
    }
    if offset < content.len() {
        spans.push(LineSpan {
            start: offset,
            end: content.len(),
        });
    }
    spans
}

fn get_replacement_line_range(
    lines: &[LineSpan],
    replacement: &TextReplacement,
) -> Result<(usize, usize), String> {
    let replacement_start = replacement.match_index;
    let replacement_end = replacement.match_index + replacement.match_length;

    let mut start_line = None;
    for (i, line) in lines.iter().enumerate() {
        if replacement_start >= line.start && replacement_start < line.end {
            start_line = Some(i);
            break;
        }
    }
    let start_line =
        start_line.ok_or_else(|| "Replacement range is outside the base content.".to_string())?;

    let mut end_line = start_line;
    while end_line < lines.len() && lines[end_line].end < replacement_end {
        end_line += 1;
    }
    if end_line >= lines.len() {
        return Err("Replacement range is outside the base content.".to_string());
    }

    Ok((start_line, end_line + 1))
}

fn apply_replacements(content: &str, replacements: &[TextReplacement], offset: usize) -> String {
    let mut result = content.to_string();
    for replacement in replacements.iter().rev() {
        let match_index = replacement.match_index.saturating_sub(offset);
        let replacement_end = match_index + replacement.match_length;
        let mut next = String::with_capacity(result.len() + replacement.new_text.len());
        next.push_str(safe_slice(&result, 0, match_index));
        next.push_str(&replacement.new_text);
        next.push_str(safe_slice(&result, replacement_end, result.len()));
        result = next;
    }
    result
}

#[derive(Clone, Debug)]
struct MatchedEdit {
    edit_index: usize,
    match_index: usize,
    match_length: usize,
    new_text: String,
}

impl From<&MatchedEdit> for TextReplacement {
    fn from(value: &MatchedEdit) -> Self {
        TextReplacement {
            match_index: value.match_index,
            match_length: value.match_length,
            new_text: value.new_text.clone(),
        }
    }
}

fn widen_replacements_to_lines(
    original_content: &str,
    base_content: &str,
    replacements: &[MatchedEdit],
) -> Result<Vec<TextReplacement>, String> {
    let original_lines = get_line_spans(original_content);
    let base_lines = get_line_spans(base_content);
    if original_lines.len() != base_lines.len() {
        return Err(
            "Cannot preserve unchanged lines because the base content has a different line count."
                .to_string(),
        );
    }

    let mut sorted: Vec<&MatchedEdit> = replacements.iter().collect();
    sorted.sort_by_key(|replacement| replacement.match_index);

    let mut groups: Vec<(usize, usize, Vec<&MatchedEdit>)> = Vec::new();
    for replacement in sorted {
        let (start_line, end_line) =
            get_replacement_line_range(&base_lines, &TextReplacement::from(replacement))?;
        if let Some(current) = groups.last_mut()
            && start_line < current.1
        {
            current.1 = current.1.max(end_line);
            current.2.push(replacement);
            continue;
        }
        groups.push((start_line, end_line, vec![replacement]));
    }

    let mut result = Vec::with_capacity(groups.len());
    for (start_line, end_line, group) in groups {
        let group_start_offset = base_lines[start_line].start;
        let group_end_offset = base_lines[end_line - 1].end;
        let original_start = original_lines[start_line].start;
        let group_replacements: Vec<TextReplacement> =
            group.iter().map(|r| TextReplacement::from(*r)).collect();
        result.push(TextReplacement {
            match_index: original_start,
            match_length: original_lines[end_line - 1].end - original_start,
            new_text: apply_replacements(
                safe_slice(base_content, group_start_offset, group_end_offset),
                &group_replacements,
                group_start_offset,
            ),
        });
    }
    Ok(result)
}

/// Apply one or more exact replacements to LF-normalized content.
pub fn apply_edits_to_normalized_content(
    normalized_content: &str,
    edits: &[Edit],
    path: &str,
) -> Result<AppliedEditsResult, String> {
    let normalized_edits: Vec<Edit> = edits
        .iter()
        .map(|edit| Edit {
            old_text: normalize_to_lf(&edit.old_text),
            new_text: normalize_to_lf(&edit.new_text),
        })
        .collect();

    for (i, edit) in normalized_edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(get_empty_old_text_error(path, i, normalized_edits.len()));
        }
    }

    let initial_matches: Vec<FuzzyMatchResult> = normalized_edits
        .iter()
        .map(|edit| fuzzy_find_text(normalized_content, &edit.old_text))
        .collect();
    let used_fuzzy_match = initial_matches.iter().any(|m| m.used_fuzzy_match);
    let replacement_base_content = if used_fuzzy_match {
        normalize_for_fuzzy_match(normalized_content)
    } else {
        normalized_content.to_string()
    };

    let mut matched_edits: Vec<MatchedEdit> = Vec::new();
    for (i, edit) in normalized_edits.iter().enumerate() {
        let match_result = fuzzy_find_text(&replacement_base_content, &edit.old_text);
        if !match_result.found {
            return Err(get_not_found_error(path, i, normalized_edits.len()));
        }
        let occurrences = count_occurrences(&replacement_base_content, &edit.old_text);
        if occurrences > 1 {
            return Err(get_duplicate_error(
                path,
                i,
                normalized_edits.len(),
                occurrences,
            ));
        }
        matched_edits.push(MatchedEdit {
            edit_index: i,
            match_index: match_result.index as usize,
            match_length: match_result.match_length,
            new_text: edit.new_text.clone(),
        });
    }

    matched_edits.sort_by_key(|m| m.match_index);
    for pair in matched_edits.windows(2) {
        let previous = &pair[0];
        let current = &pair[1];
        if previous.match_index + previous.match_length > current.match_index {
            return Err(format!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.",
                previous.edit_index, current.edit_index
            ));
        }
    }

    let base_content = normalized_content.to_string();
    let replacements = if used_fuzzy_match {
        widen_replacements_to_lines(
            normalized_content,
            &replacement_base_content,
            &matched_edits,
        )?
    } else {
        matched_edits.iter().map(TextReplacement::from).collect()
    };
    let new_content = apply_replacements(&base_content, &replacements, 0);

    if base_content == new_content {
        return Err(get_no_change_error(path, normalized_edits.len()));
    }

    Ok(AppliedEditsResult {
        base_content,
        new_content,
        replacements,
    })
}

struct CrlfMapper<'a> {
    content: &'a str,
    crlf_before: usize,
    next_crlf: Option<usize>,
}

impl<'a> CrlfMapper<'a> {
    fn new(content: &'a str) -> Self {
        CrlfMapper {
            content,
            crlf_before: 0,
            next_crlf: content.find("\r\n"),
        }
    }

    fn map_to_original(&mut self, offset: usize) -> usize {
        while let Some(next) = self.next_crlf {
            if next - self.crlf_before < offset {
                self.crlf_before += 1;
                self.next_crlf = self.content[next + 2..].find("\r\n").map(|p| next + 2 + p);
            } else {
                break;
            }
        }
        offset + self.crlf_before
    }
}

fn get_line_ending_at(content: &str, offset: usize) -> &'static str {
    let mut line_feed = content
        .get(offset..)
        .and_then(|tail| tail.find('\n'))
        .map(|p| offset + p);
    if line_feed.is_none() {
        line_feed = safe_slice(content, 0, offset).rfind('\n');
    }
    match line_feed {
        Some(index) if index > 0 && content.as_bytes().get(index - 1) == Some(&b'\r') => "\r\n",
        Some(_) => "\n",
        None if content.contains('\r') => "\r",
        None => "\n",
    }
}

/// Splice replacements matched in LF-normalized content back into the original,
/// preserving untouched line endings and converting inserted line breaks.
pub fn apply_replacements_to_original(
    original_content: &str,
    replacements: &[TextReplacement],
) -> String {
    let mut mapper = CrlfMapper::new(original_content);
    let mut parts: Vec<String> = Vec::new();
    let mut ends_with_cr = false;

    let mut push = |part: &str| {
        if part.is_empty() {
            return;
        }
        if ends_with_cr && part.starts_with('\n') {
            parts.push("\n".to_string());
        }
        parts.push(part.to_string());
        ends_with_cr = part.ends_with('\r');
    };

    let mut position = 0;
    for replacement in replacements {
        let start = mapper.map_to_original(replacement.match_index);
        push(safe_slice(original_content, position, start));
        let ending = get_line_ending_at(original_content, start);
        let new_text = if ending == "\n" {
            replacement.new_text.clone()
        } else {
            replacement.new_text.replace('\n', ending)
        };
        push(&new_text);
        position = mapper.map_to_original(replacement.match_index + replacement.match_length);
    }
    push(safe_slice(
        original_content,
        position,
        original_content.len(),
    ));
    parts.concat()
}

fn count_line_feeds(text: &str, start: usize, end: usize) -> usize {
    safe_slice(text, start, end)
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
}

fn count_lines(text: &str) -> usize {
    count_line_feeds(text, 0, text.len()) + 1
}

#[derive(Clone, Debug)]
struct DiffPart {
    value: String,
    added: bool,
    removed: bool,
}

/// Convert similar's per-line changes into jsdiff-style grouped parts.
fn diff_parts(old: &str, new: &str) -> Vec<DiffPart> {
    let diff = TextDiff::from_lines(old, new);
    let mut parts: Vec<DiffPart> = Vec::new();
    for change in diff.iter_all_changes() {
        let (added, removed) = match change.tag() {
            ChangeTag::Equal => (false, false),
            ChangeTag::Delete => (false, true),
            ChangeTag::Insert => (true, false),
        };
        if let Some(last) = parts.last_mut()
            && last.added == added
            && last.removed == removed
        {
            last.value.push_str(change.value());
            continue;
        }
        parts.push(DiffPart {
            value: change.value().to_string(),
            added,
            removed,
        });
    }
    parts
}

fn pad_number(value: usize, width: usize) -> String {
    format!("{value:>width$}")
}

fn skip_marker(width: usize) -> String {
    format!(" {} ...", " ".repeat(width))
}

fn format_display_diff(
    parts: &[DiffPart],
    old_start_line: usize,
    new_start_line: usize,
    line_num_width: usize,
    context_lines: usize,
) -> (Vec<String>, Option<usize>) {
    let mut output: Vec<String> = Vec::new();
    let mut old_line_num = old_start_line;
    let mut new_line_num = new_start_line;
    let mut last_was_change = false;
    let mut first_changed_line: Option<usize> = None;

    for (i, part) in parts.iter().enumerate() {
        let mut raw: Vec<&str> = part.value.split('\n').collect();
        if raw.last() == Some(&"") {
            raw.pop();
        }

        if part.added || part.removed {
            if first_changed_line.is_none() {
                first_changed_line = Some(new_line_num);
            }
            for line in raw {
                if part.added {
                    output.push(format!(
                        "+{} {line}",
                        pad_number(new_line_num, line_num_width)
                    ));
                    new_line_num += 1;
                } else {
                    output.push(format!(
                        "-{} {line}",
                        pad_number(old_line_num, line_num_width)
                    ));
                    old_line_num += 1;
                }
            }
            last_was_change = true;
            continue;
        }

        let next_part_is_change =
            i + 1 < parts.len() && (parts[i + 1].added || parts[i + 1].removed);
        let has_leading_change = last_was_change;
        let has_trailing_change = next_part_is_change;

        if has_leading_change && has_trailing_change {
            if raw.len() <= context_lines * 2 {
                for line in raw {
                    output.push(format!(
                        " {} {line}",
                        pad_number(old_line_num, line_num_width)
                    ));
                    old_line_num += 1;
                    new_line_num += 1;
                }
            } else {
                for line in &raw[..context_lines] {
                    output.push(format!(
                        " {} {line}",
                        pad_number(old_line_num, line_num_width)
                    ));
                    old_line_num += 1;
                    new_line_num += 1;
                }
                let skipped = raw.len() - context_lines * 2;
                output.push(skip_marker(line_num_width));
                old_line_num += skipped;
                new_line_num += skipped;
                for line in &raw[raw.len() - context_lines..] {
                    output.push(format!(
                        " {} {line}",
                        pad_number(old_line_num, line_num_width)
                    ));
                    old_line_num += 1;
                    new_line_num += 1;
                }
            }
        } else if has_leading_change {
            let shown = context_lines.min(raw.len());
            for line in &raw[..shown] {
                output.push(format!(
                    " {} {line}",
                    pad_number(old_line_num, line_num_width)
                ));
                old_line_num += 1;
                new_line_num += 1;
            }
            let skipped = raw.len() - shown;
            if skipped > 0 {
                output.push(skip_marker(line_num_width));
                old_line_num += skipped;
                new_line_num += skipped;
            }
        } else if has_trailing_change {
            let skipped = raw.len().saturating_sub(context_lines);
            if skipped > 0 {
                output.push(skip_marker(line_num_width));
                old_line_num += skipped;
                new_line_num += skipped;
            }
            for line in &raw[skipped..] {
                output.push(format!(
                    " {} {line}",
                    pad_number(old_line_num, line_num_width)
                ));
                old_line_num += 1;
                new_line_num += 1;
            }
        } else {
            old_line_num += raw.len();
            new_line_num += raw.len();
        }

        last_was_change = false;
    }

    (output, first_changed_line)
}

/// Standard unified patch (`--- path\n+++ path\n`, context radius default 4).
pub fn generate_unified_patch(
    path: &str,
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> String {
    let diff = TextDiff::from_lines(old_content, new_content);
    let patch = diff
        .unified_diff()
        .context_radius(context_lines)
        .header(path, path)
        .to_string();
    if patch.is_empty() {
        format!("--- {path}\n+++ {path}\n")
    } else {
        patch
    }
}

/// Display diff with line numbers and context, plus the first changed new-file line.
pub fn generate_diff_string(
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> DiffStringResult {
    let line_num_width = count_lines(old_content)
        .max(count_lines(new_content))
        .to_string()
        .len();
    let parts = diff_parts(old_content, new_content);
    let (output, first_changed_line) =
        format_display_diff(&parts, 1, 1, line_num_width, context_lines);
    DiffStringResult {
        diff: output.join("\n"),
        first_changed_line,
    }
}

fn line_start_before(text: &str, offset: usize) -> usize {
    if offset == 0 {
        return 0;
    }
    safe_slice(text, 0, offset)
        .rfind('\n')
        .map_or(0, |index| index + 1)
}

#[derive(Clone, Copy, Debug)]
struct DiffWindow {
    old_start: usize,
    old_end: usize,
    old_line: usize,
    new_start: usize,
    new_end: usize,
    new_line: usize,
}

fn get_diff_windows(base_content: &str, replacements: &[TextReplacement]) -> Vec<DiffWindow> {
    let mut windows: Vec<DiffWindow> = Vec::new();
    let mut offset_delta = 0isize;
    let mut line_delta = 0isize;
    let mut cursor_offset = 0usize;
    let mut cursor_line = 0usize;

    for replacement in replacements {
        let match_end = replacement.match_index + replacement.match_length;
        let mut start = line_start_before(base_content, replacement.match_index);
        for _ in 0..DIFF_WINDOW_MARGIN_LINES {
            if start == 0 {
                break;
            }
            start = line_start_before(base_content, start - 1);
        }

        let mut end = match_end;
        for i in 0..=DIFF_WINDOW_MARGIN_LINES {
            if end >= base_content.len() {
                break;
            }
            let search_from = if i == 0 {
                match_end.max(replacement.match_index).saturating_sub(1)
            } else {
                end
            };
            let line_feed = safe_slice(base_content, search_from, base_content.len())
                .find('\n')
                .map(|p| search_from + p);
            end = line_feed.map_or(base_content.len(), |p| p + 1);
        }

        let replacement_offset_delta =
            replacement.new_text.len() as isize - replacement.match_length as isize;
        let replacement_line_delta =
            count_line_feeds(&replacement.new_text, 0, replacement.new_text.len()) as isize
                - count_line_feeds(base_content, replacement.match_index, match_end) as isize;

        if let Some(previous) = windows.last_mut()
            && start <= previous.old_end
        {
            previous.old_end = previous.old_end.max(end);
            previous.new_end = (previous.old_end as isize + offset_delta + replacement_offset_delta)
                .max(0) as usize;
            offset_delta += replacement_offset_delta;
            line_delta += replacement_line_delta;
            continue;
        }

        cursor_line += count_line_feeds(base_content, cursor_offset, start);
        cursor_offset = start;
        windows.push(DiffWindow {
            old_start: start,
            old_end: end,
            old_line: cursor_line,
            new_start: (start as isize + offset_delta).max(0) as usize,
            new_end: (end as isize + offset_delta + replacement_offset_delta).max(0) as usize,
            new_line: (cursor_line as isize + line_delta).max(0) as usize,
        });
        offset_delta += replacement_offset_delta;
        line_delta += replacement_line_delta;
    }

    windows
}

fn shift_hunk_header(line: &str, old_shift: usize, new_shift: usize) -> String {
    let trimmed = line.trim_end_matches(['\n', '\r']);
    let Some(rest) = trimmed.strip_prefix("@@ -") else {
        return line.to_string();
    };
    let Some((old_part, remainder)) = rest.split_once(" +") else {
        return line.to_string();
    };
    let Some((new_part, _)) = remainder.split_once(" @@") else {
        return line.to_string();
    };

    let shift_range = |range: &str, shift: usize| -> String {
        let (start_str, len_str) = match range.split_once(',') {
            Some((start, len)) => (start, Some(len)),
            None => (range, None),
        };
        let Ok(start) = start_str.parse::<usize>() else {
            return range.to_string();
        };
        let shifted = start + shift;
        match len_str {
            Some(len) => format!("{shifted},{len}"),
            None => shifted.to_string(),
        }
    };

    format!(
        "@@ -{} +{} @@\n",
        shift_range(old_part, old_shift),
        shift_range(new_part, new_shift)
    )
}

fn unified_hunks_with_shift(
    old: &str,
    new: &str,
    context_lines: usize,
    old_shift: usize,
    new_shift: usize,
) -> String {
    let diff = TextDiff::from_lines(old, new);
    let text = diff
        .unified_diff()
        .context_radius(context_lines)
        .to_string();
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        if line.starts_with("@@ -") {
            out.push_str(&shift_hunk_header(line, old_shift, new_shift));
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Diff only around the replacements so cost follows the change, not the file.
pub fn generate_edit_diff(
    path: &str,
    base_content: &str,
    new_content: &str,
    replacements: &[TextReplacement],
    context_lines: usize,
) -> EditDiff {
    let line_num_width = count_lines(base_content)
        .max(count_lines(new_content))
        .to_string()
        .len();
    let marker = skip_marker(line_num_width);
    let mut output: Vec<String> = Vec::new();
    let mut hunks = String::new();
    let mut first_changed_line: Option<usize> = None;

    for window in get_diff_windows(base_content, replacements) {
        let old_text = safe_slice(base_content, window.old_start, window.old_end);
        let new_text = safe_slice(new_content, window.new_start, window.new_end);
        let parts = diff_parts(old_text, new_text);
        let (mut shown, shown_first) = format_display_diff(
            &parts,
            window.old_line + 1,
            window.new_line + 1,
            line_num_width,
            context_lines,
        );
        if first_changed_line.is_none() {
            first_changed_line = shown_first;
        }
        if output.last() == Some(&marker) && shown.first() == Some(&marker) {
            shown.remove(0);
        }
        for line in shown {
            output.push(line);
        }
        hunks.push_str(&unified_hunks_with_shift(
            old_text,
            new_text,
            context_lines,
            window.old_line,
            window.new_line,
        ));
    }

    EditDiff {
        diff: output.join("\n"),
        patch: format!("--- {path}\n+++ {path}\n{hunks}"),
        first_changed_line,
    }
}

fn io_error_code(error: &std::io::Error) -> String {
    match error.raw_os_error() {
        Some(2) => "ENOENT".to_string(),
        Some(13) => "EACCES".to_string(),
        Some(21) => "EISDIR".to_string(),
        Some(20) => "ENOTDIR".to_string(),
        Some(code) => format!("E{code}"),
        None => error.kind().to_string(),
    }
}

/// `computeEditsDiff` (sync variant): resolve, read, strip BOM, apply, diff.
pub fn compute_edits_diff(
    path: &str,
    edits: &[Edit],
    cwd: &Path,
) -> Result<EditDiffResult, String> {
    let absolute_path = resolve_to_cwd(path, cwd);
    let raw = fs::read(&absolute_path).map_err(|error| {
        format!(
            "Could not edit file: {path}. Error code: {}.",
            io_error_code(&error)
        )
    })?;
    let raw_content = String::from_utf8_lossy(&raw).into_owned();
    let (_, content) = split_bom(&raw_content);
    let normalized_content = normalize_to_lf(&content);
    let applied = apply_edits_to_normalized_content(&normalized_content, edits, path)?;
    let diff = generate_edit_diff(
        path,
        &applied.base_content,
        &applied.new_content,
        &applied.replacements,
        4,
    );
    Ok(EditDiffResult {
        diff: diff.diff,
        first_changed_line: diff.first_changed_line,
    })
}

/// `computeEditDiff` (sync variant): single-edit convenience wrapper.
pub fn compute_edit_diff(
    path: &str,
    old_text: &str,
    new_text: &str,
    cwd: &Path,
) -> Result<EditDiffResult, String> {
    compute_edits_diff(
        path,
        &[Edit {
            old_text: old_text.to_string(),
            new_text: new_text.to_string(),
        }],
        cwd,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_to_lf_handles_crlf_and_lone_cr() {
        assert_eq!(normalize_to_lf("a\r\nb\rc"), "a\nb\nc");
        assert_eq!(split_bom("\u{feff}hello").0, "\u{feff}");
        assert_eq!(split_bom("hello").0, "");
    }

    #[test]
    fn normalize_for_fuzzy_match_maps_smart_punctuation() {
        assert_eq!(normalize_for_fuzzy_match("a’b"), "a'b");
        assert_eq!(normalize_for_fuzzy_match("a—b"), "a-b");
        assert_eq!(normalize_for_fuzzy_match("a\u{a0}b"), "a b");
        assert_eq!(normalize_for_fuzzy_match("ＡＢＣ"), "ABC");
    }

    #[test]
    fn fuzzy_find_text_prefers_exact_match() {
        let content = "a'b\n";
        let result = fuzzy_find_text(content, "a'b");
        assert!(result.found);
        assert!(!result.used_fuzzy_match);
        assert_eq!(result.index, 0);
        assert_eq!(result.match_length, 3);
    }

    #[test]
    fn fuzzy_find_text_matches_curly_quote() {
        let content = "a’b\n";
        let result = fuzzy_find_text(content, "a'b");
        assert!(result.found);
        assert!(result.used_fuzzy_match);
        assert_eq!(result.content_for_replacement, "a'b\n");
    }

    #[test]
    fn fuzzy_find_text_not_found() {
        let result = fuzzy_find_text("abc", "zzz");
        assert!(!result.found);
        assert_eq!(result.index, -1);
    }

    fn error_of(result: Result<AppliedEditsResult, String>) -> String {
        match result {
            Ok(_) => "<unexpected success>".to_string(),
            Err(error) => error,
        }
    }

    #[test]
    fn apply_edits_single_replacement() -> Result<(), String> {
        let applied = apply_edits_to_normalized_content(
            "hello world\n",
            &[Edit {
                old_text: "world".into(),
                new_text: "testing".into(),
            }],
            "f.txt",
        )?;
        assert_eq!(applied.new_content, "hello testing\n");
        assert_eq!(applied.replacements.len(), 1);
        Ok(())
    }

    #[test]
    fn apply_edits_duplicate_error_message() {
        let error = error_of(apply_edits_to_normalized_content(
            "foo foo foo",
            &[Edit {
                old_text: "foo".into(),
                new_text: "bar".into(),
            }],
            "f.txt",
        ));
        assert_eq!(
            error,
            "Found 3 occurrences of the text in f.txt. The text must be unique. Please provide more context to make it unique."
        );
    }

    #[test]
    fn apply_edits_empty_old_text_error() {
        let error = error_of(apply_edits_to_normalized_content(
            "abc",
            &[Edit {
                old_text: "".into(),
                new_text: "x".into(),
            }],
            "f.txt",
        ));
        assert_eq!(error, "oldText must not be empty in f.txt.");
    }

    #[test]
    fn apply_edits_no_change_error() {
        let error = error_of(apply_edits_to_normalized_content(
            "abc",
            &[Edit {
                old_text: "abc".into(),
                new_text: "abc".into(),
            }],
            "f.txt",
        ));
        assert!(error.starts_with("No changes made to f.txt."));
    }

    #[test]
    fn apply_edits_overlap_error() {
        let error = error_of(apply_edits_to_normalized_content(
            "abcdef",
            &[
                Edit {
                    old_text: "abc".into(),
                    new_text: "X".into(),
                },
                Edit {
                    old_text: "bcd".into(),
                    new_text: "Y".into(),
                },
            ],
            "f.txt",
        ));
        assert!(error.contains("overlap in f.txt"));
    }

    #[test]
    fn apply_edits_multi_disjoint() -> Result<(), String> {
        let applied = apply_edits_to_normalized_content(
            "alpha\nbeta\ngamma\ndelta\n",
            &[
                Edit {
                    old_text: "alpha".into(),
                    new_text: "ALPHA".into(),
                },
                Edit {
                    old_text: "gamma".into(),
                    new_text: "GAMMA".into(),
                },
            ],
            "f.txt",
        )?;
        assert_eq!(applied.new_content, "ALPHA\nbeta\nGAMMA\ndelta\n");
        Ok(())
    }

    #[test]
    fn apply_replacements_preserves_crlf() {
        let original = "first\r\nsecond\r\nthird\r\n";
        let replacements = vec![TextReplacement {
            match_index: 6,
            match_length: 6,
            new_text: "REPLACED".into(),
        }];
        let result = apply_replacements_to_original(original, &replacements);
        assert_eq!(result, "first\r\nREPLACED\r\nthird\r\n");
    }

    #[test]
    fn apply_replacements_preserves_bom_seam() {
        let (bom, content) = split_bom("\u{feff}abc\nxyz");
        assert_eq!(bom, "\u{feff}");
        let result = format!("{bom}{}", apply_replacements_to_original(&content, &[]));
        assert!(result.starts_with('\u{feff}'));
    }

    #[test]
    fn generate_unified_patch_has_headers_and_hunks() {
        let patch = generate_unified_patch("f.txt", "a\nb\nc\n", "a\nB\nc\n", 4);
        assert!(patch.starts_with("--- f.txt\n+++ f.txt\n"));
        assert!(patch.contains("@@ -1,3 +1,3 @@"));
        assert!(patch.contains("-b\n+B\n"));
        assert!(patch.ends_with('\n'));
    }

    #[test]
    fn generate_unified_patch_missing_newline_marker() {
        let patch = generate_unified_patch("f.txt", "a\n", "b", 4);
        assert!(patch.contains("\\ No newline at end of file"));
    }

    #[test]
    fn generate_diff_string_marks_first_changed_line() {
        let result = generate_diff_string("one\ntwo\nthree\n", "one\nTWO\nthree\n", 4);
        assert_eq!(result.first_changed_line, Some(2));
        assert!(result.diff.contains("-2 two"));
        assert!(result.diff.contains("+2 TWO"));
    }

    #[test]
    fn generate_diff_string_uses_skip_marker_for_large_gap() {
        let old = (1..=20)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = old.replacen("line 1", "LINE 1", 1);
        let result = generate_diff_string(&old, &new, 4);
        assert!(result.diff.contains(" ..."));
        assert!(!result.diff.contains(" line 10"));
    }

    #[test]
    fn generated_patch_applies_to_file() -> std::io::Result<()> {
        let has_patch = std::process::Command::new("patch")
            .arg("--version")
            .status()
            .is_ok_and(|status| status.success());
        if !has_patch {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let base = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";
        std::fs::write(dir.path().join("f.txt"), base)?;
        let replacements = vec![TextReplacement {
            match_index: 4,
            match_length: 3,
            new_text: "TWO".into(),
        }];
        let new_content = apply_replacements(base, &replacements, 0);
        let edit_diff = generate_edit_diff("f.txt", base, &new_content, &replacements, 4);
        std::fs::write(dir.path().join("p.patch"), edit_diff.patch)?;
        let status = std::process::Command::new("patch")
            .arg("-p0")
            .arg("-i")
            .arg("p.patch")
            .current_dir(dir.path())
            .status()?;
        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt"))?,
            new_content
        );
        Ok(())
    }

    #[test]
    fn patch_hunk_numbers_shift_for_late_replacement() -> std::io::Result<()> {
        let has_patch = std::process::Command::new("patch")
            .arg("--version")
            .status()
            .is_ok_and(|status| status.success());
        if !has_patch {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let base = (1..=100)
            .map(|number| format!("line {number}\n"))
            .collect::<String>();
        std::fs::write(dir.path().join("f.txt"), &base)?;
        let match_index = base
            .find("line 60\n")
            .ok_or_else(|| std::io::Error::other("missing line"))?;
        let replacements = vec![TextReplacement {
            match_index,
            match_length: "line 60".len(),
            new_text: "LINE 60".into(),
        }];
        let new_content = apply_replacements(&base, &replacements, 0);
        let edit_diff = generate_edit_diff("f.txt", &base, &new_content, &replacements, 4);
        assert!(edit_diff.patch.contains("@@ -"));
        std::fs::write(dir.path().join("p.patch"), edit_diff.patch)?;
        let status = std::process::Command::new("patch")
            .arg("-p0")
            .arg("-i")
            .arg("p.patch")
            .current_dir(dir.path())
            .status()?;
        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt"))?,
            new_content
        );
        Ok(())
    }

    #[test]
    fn generate_edit_diff_patch_matches_display_diff() {
        let base = "one\ntwo\nthree\n";
        let replacements = vec![TextReplacement {
            match_index: 4,
            match_length: 3,
            new_text: "TWO".into(),
        }];
        let new = apply_replacements(base, &replacements, 0);
        let edit_diff = generate_edit_diff("f.txt", base, &new, &replacements, 4);
        let display = generate_diff_string(base, &new, 4);
        assert_eq!(edit_diff.diff, display.diff);
        assert_eq!(edit_diff.first_changed_line, display.first_changed_line);
        assert!(edit_diff.patch.starts_with("--- f.txt\n+++ f.txt\n"));
    }
}
