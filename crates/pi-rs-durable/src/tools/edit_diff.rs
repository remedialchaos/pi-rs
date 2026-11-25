//! Exact and fuzzy replacements, display diffs and unified patches.

use std::{borrow::Cow, fmt::Write, ops::Range};

use serde::{Deserialize, Serialize};
use similar::{ChangeTag, DiffTag, TextDiff};

use crate::unicode::nfkc;

use super::{ToolError, is_js_space};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Edit {
    pub old_text: String,
    pub new_text: String,
}

pub fn detect_line_ending(content: &str) -> &'static str {
    if content
        .find('\n')
        .is_some_and(|index| index > 0 && content.as_bytes()[index - 1] == b'\r')
    {
        "\r\n"
    } else {
        "\n"
    }
}

pub fn normalize_to_lf(content: &str) -> Cow<'_, str> {
    if !content.contains('\r') {
        return Cow::Borrowed(content);
    }
    let mut result = String::with_capacity(content.len());
    let mut parts = content.split('\r');
    if let Some(first) = parts.next() {
        result.push_str(first);
    }
    for part in parts {
        result.push('\n');
        result.push_str(part.strip_prefix('\n').unwrap_or(part));
    }
    Cow::Owned(result)
}

pub fn restore_line_endings<'a>(content: &'a str, ending: &str) -> Cow<'a, str> {
    if ending == "\r\n" {
        Cow::Owned(content.replace('\n', "\r\n"))
    } else {
        Cow::Borrowed(content)
    }
}

pub fn strip_bom(content: &str) -> (&str, &str) {
    content
        .strip_prefix('\u{feff}')
        .map_or(("", content), |text| ("\u{feff}", text))
}

fn fuzzy_char(ch: char) -> char {
    match ch {
        '\u{2018}'..='\u{201b}' => '\'',
        '\u{201c}'..='\u{201f}' => '"',
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
        '\u{a0}' | '\u{2002}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => ' ',
        _ => ch,
    }
}

pub fn normalize_for_fuzzy_match(content: &str) -> Cow<'_, str> {
    let normalized = nfkc(content);
    let needs_change = normalized
        .split('\n')
        .any(|line| line.ends_with(is_js_space) || line.chars().any(|ch| fuzzy_char(ch) != ch));
    if !needs_change {
        return normalized;
    }
    let mut result = String::with_capacity(normalized.len());
    for (index, line) in normalized.split('\n').enumerate() {
        if index != 0 {
            result.push('\n');
        }
        result.extend(line.trim_end_matches(is_js_space).chars().map(fuzzy_char));
    }
    Cow::Owned(result)
}

/// Offsets are UTF-8 byte indexes into `content_for_replacement`, not the original on a fuzzy match.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FuzzyMatch<'a> {
    pub range: Range<usize>,
    pub used_fuzzy_match: bool,
    pub content_for_replacement: Cow<'a, str>,
}

pub fn fuzzy_find_text<'a>(content: &'a str, old_text: &str) -> Option<FuzzyMatch<'a>> {
    if let Some(start) = content.find(old_text) {
        return Some(FuzzyMatch {
            range: start..start + old_text.len(),
            used_fuzzy_match: false,
            content_for_replacement: Cow::Borrowed(content),
        });
    }
    let base = normalize_for_fuzzy_match(content);
    let old_text = normalize_for_fuzzy_match(old_text);
    let start = base.find(old_text.as_ref())?;
    Some(FuzzyMatch {
        range: start..start + old_text.len(),
        used_fuzzy_match: true,
        content_for_replacement: base,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppliedEdits<'a> {
    pub base_content: &'a str,
    pub new_content: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextReplacement<'a> {
    pub range: Range<usize>,
    pub new_text: &'a str,
}

fn append_replacements(
    output: &mut String,
    base: &str,
    span: Range<usize>,
    edits: &[TextReplacement<'_>],
) -> Result<(), ToolError> {
    let outside = || ToolError::Message("Replacement range is outside the base content.".into());
    let mut cursor = span.start;
    for edit in edits {
        if edit.range.end > span.end
            || edit.range.start > edit.range.end
            || !base.is_char_boundary(edit.range.end)
        {
            return Err(outside());
        }
        output.push_str(base.get(cursor..edit.range.start).ok_or_else(outside)?);
        output.push_str(edit.new_text);
        cursor = edit.range.end;
    }
    output.push_str(base.get(cursor..span.end).ok_or_else(outside)?);
    Ok(())
}

fn line_offsets(content: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    let mut offset = 0;
    for line in content.split_inclusive('\n') {
        offset += line.len();
        offsets.push(offset);
    }
    offsets
}

/// Use the normalized lines only where an edit touched them; all other lines keep their original bytes.
pub fn apply_replacements_preserving_unchanged_lines(
    original: &str,
    base: &str,
    edits: &[TextReplacement<'_>],
) -> Result<String, ToolError> {
    let original_offsets = line_offsets(original);
    let base_offsets = line_offsets(base);
    if original_offsets.len() != base_offsets.len() {
        return Err(ToolError::Message(
            "Cannot preserve unchanged lines because the base content has a different line count."
                .into(),
        ));
    }
    let mut sorted;
    let edits = if edits.is_sorted_by_key(|edit| edit.range.start) {
        edits
    } else {
        sorted = edits.to_vec();
        sorted.sort_by_key(|edit| edit.range.start);
        &sorted
    };
    let lines = |edit: &TextReplacement<'_>| {
        if edit.range.start >= base.len()
            || edit.range.end > base.len()
            || edit.range.end < edit.range.start
        {
            return Err(ToolError::Message(
                "Replacement range is outside the base content.".into(),
            ));
        }
        let start = base_offsets.partition_point(|&offset| offset <= edit.range.start) - 1;
        let end = base_offsets
            .partition_point(|&offset| offset < edit.range.end)
            .max(start + 1);
        Ok(start..end)
    };
    let mut output = String::with_capacity(original.len());
    let mut cursor = 0;
    let mut index = 0;
    while let Some(edit) = edits.get(index) {
        let range = lines(edit)?;
        let mut end = range.end;
        let first = index;
        index += 1;
        while let Some(next) = edits.get(index) {
            let next_range = lines(next)?;
            if next_range.start >= end {
                break;
            }
            end = end.max(next_range.end);
            index += 1;
        }
        output.push_str(&original[original_offsets[cursor]..original_offsets[range.start]]);
        append_replacements(
            &mut output,
            base,
            base_offsets[range.start]..base_offsets[end],
            &edits[first..index],
        )?;
        cursor = end;
    }
    output.push_str(&original[original_offsets[cursor]..]);
    Ok(output)
}

pub fn apply_edits_to_normalized_content<'a>(
    content: &'a str,
    edits: &[Edit],
    path: &str,
) -> Result<AppliedEdits<'a>, ToolError> {
    let single = edits.len() == 1;
    let mut inputs = Vec::with_capacity(edits.len());
    let mut fuzzy = false;
    let normalized = normalize_for_fuzzy_match(content);
    for (index, edit) in edits.iter().enumerate() {
        let old = normalize_to_lf(&edit.old_text);
        if old.is_empty() {
            let field = if single {
                "oldText".into()
            } else {
                format!("edits[{index}].oldText")
            };
            return Err(ToolError::Message(format!(
                "{field} must not be empty in {path}."
            )));
        }
        let norm_old = normalize_for_fuzzy_match(&old);
        let exact = content.find(old.as_ref());
        fuzzy |= exact.is_none() && normalized.contains(norm_old.as_ref());
        let norm_old = match norm_old {
            Cow::Borrowed(_) => None,
            Cow::Owned(text) => Some(text),
        };
        inputs.push((old, exact, norm_old, normalize_to_lf(&edit.new_text)));
    }
    let mut matches = Vec::with_capacity(edits.len());
    for (index, (raw, exact, normalized_old, new)) in inputs.iter().enumerate() {
        let old = normalized_old.as_deref().unwrap_or(raw);
        let (start, length) = if fuzzy {
            let raw: &str = raw;
            normalized.find(raw).map_or_else(
                || (normalized.find(old), old.len()),
                |start| (Some(start), raw.len()),
            )
        } else {
            (*exact, raw.len())
        };
        let Some(start) = start else {
            return Err(ToolError::Message(if single {
                format!(
                    "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
                )
            } else {
                format!(
                    "Could not find edits[{index}] in {path}. The oldText must match exactly including all whitespace and newlines."
                )
            }));
        };
        let occurrences = if old.is_empty() {
            normalized.encode_utf16().count().saturating_sub(1)
        } else {
            normalized.matches(old).count()
        };
        if occurrences > 1 {
            return Err(ToolError::Message(if single {
                format!(
                    "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
                )
            } else {
                format!(
                    "Found {occurrences} occurrences of edits[{index}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
                )
            }));
        }
        matches.push((
            index,
            TextReplacement {
                range: start..start + length,
                new_text: new,
            },
        ));
    }
    matches.sort_unstable_by_key(|(index, edit)| (edit.range.start, *index));
    for pair in matches.windows(2) {
        let [(a, left), (b, right)] = pair else {
            continue;
        };
        if left.range.end > right.range.start {
            return Err(ToolError::Message(format!(
                "edits[{a}] and edits[{b}] overlap in {path}. Merge them into one edit or target disjoint regions."
            )));
        }
    }
    let replacements: Vec<_> = matches.into_iter().map(|(_, edit)| edit).collect();
    let new_content = if fuzzy {
        apply_replacements_preserving_unchanged_lines(content, &normalized, &replacements)?
    } else {
        let mut output = String::with_capacity(content.len());
        append_replacements(&mut output, content, 0..content.len(), &replacements)?;
        output
    };
    if new_content == content {
        return Err(ToolError::Message(if single {
            format!(
                "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
            )
        } else {
            format!("No changes made to {path}. The replacements produced identical content.")
        }));
    }
    Ok(AppliedEdits {
        base_content: content,
        new_content,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisplayDiff {
    pub diff: String,
    pub first_changed_line: Option<usize>,
}

fn render_patch(path: &str, diff: &TextDiff<'_, '_, str>, context_lines: usize) -> String {
    let mut patch = format!("--- {path}\n+++ {path}\n");
    for group in diff.grouped_ops(context_lines) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        let old_start = first.old_range().start;
        let new_start = first.new_range().start;
        let old_len = last.old_range().end - old_start;
        let new_len = last.new_range().end - new_start;
        let old_line = old_start + usize::from(old_len != 0);
        let new_line = new_start + usize::from(new_len != 0);
        let _ = writeln!(patch, "@@ -{old_line},{old_len} +{new_line},{new_len} @@");
        for change in group.iter().flat_map(|op| diff.iter_changes(op)) {
            patch.push(match change.tag() {
                ChangeTag::Delete => '-',
                ChangeTag::Insert => '+',
                ChangeTag::Equal => ' ',
            });
            patch.push_str(change.value());
            if change.missing_newline() {
                patch.push_str("\n\\ No newline at end of file\n");
            }
        }
    }
    patch
}

fn display_line(output: &mut String, prefix: char, line: usize, width: usize, text: &str) {
    if !output.is_empty() {
        output.push('\n');
    }
    let _ = write!(
        output,
        "{prefix}{line:>width$} {}",
        text.strip_suffix('\n').unwrap_or(text)
    );
}

fn display_skip(output: &mut String, width: usize) {
    if !output.is_empty() {
        output.push('\n');
    }
    let _ = write!(output, " {:width$} ...", "");
}

fn render_display(diff: &TextDiff<'_, '_, str>, context: usize) -> DisplayDiff {
    let count = |length: usize, last: Option<&str>| {
        length + usize::from(last.is_none_or(|line| line.ends_with('\n')))
    };
    let old = diff.old_len();
    let new = diff.new_len();
    let width = count(old, diff.old_slice(old.saturating_sub(1)))
        .max(count(new, diff.new_slice(new.saturating_sub(1))))
        .to_string()
        .len();
    let mut output = String::new();
    let mut first_changed_line = None;
    let ops = diff.ops();
    for (index, op) in ops.iter().enumerate() {
        match op.tag() {
            DiffTag::Equal => {
                let leading = index > 0;
                let trailing = index + 1 < ops.len();
                let range = op.old_range();
                let count = range.len();
                let head = if leading { count.min(context) } else { 0 };
                let tail = if trailing {
                    (count - head).min(context)
                } else {
                    0
                };
                for at in range.start..range.start + head {
                    display_line(&mut output, ' ', at + 1, width, &diff.old_lookup()[at]);
                }
                if (leading || trailing) && count > head + tail {
                    display_skip(&mut output, width);
                }
                for at in range.end - tail..range.end {
                    display_line(&mut output, ' ', at + 1, width, &diff.old_lookup()[at]);
                }
            }
            _ => {
                first_changed_line.get_or_insert(op.new_range().start + 1);
                for change in diff.iter_changes(op) {
                    match change.tag() {
                        ChangeTag::Delete => {
                            if let Some(at) = change.old_index() {
                                display_line(&mut output, '-', at + 1, width, change.value());
                            }
                        }
                        ChangeTag::Insert => {
                            if let Some(at) = change.new_index() {
                                display_line(&mut output, '+', at + 1, width, change.value());
                            }
                        }
                        ChangeTag::Equal => {}
                    }
                }
            }
        }
    }
    DisplayDiff {
        diff: output,
        first_changed_line,
    }
}

fn line_diff<'old, 'new>(
    old_content: &'old str,
    new_content: &'new str,
) -> TextDiff<'old, 'new, str> {
    TextDiff::from_lines(old_content, new_content)
}

pub fn generate_unified_patch(
    path: &str,
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> String {
    render_patch(path, &line_diff(old_content, new_content), context_lines)
}

pub fn generate_diff_string(
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> DisplayDiff {
    render_display(&line_diff(old_content, new_content), context_lines)
}

pub(super) fn generate_edit_diffs(path: &str, old: &str, new: &str) -> (DisplayDiff, String) {
    let diff = line_diff(old, new);
    (render_display(&diff, 4), render_patch(path, &diff, 4))
}
