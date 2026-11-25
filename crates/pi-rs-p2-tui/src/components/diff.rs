use std::{borrow::Cow, fmt::Write as _};

use similar::{ChangeTag, TextDiff};

use crate::theme::Theme;
#[derive(Debug, Default, Clone)]
pub struct RenderDiffOptions {
    pub file_path: Option<String>,
}

struct Line<'a> {
    prefix: u8,
    number: &'a str,
    text: &'a str,
}

fn parse(line: &str) -> Option<Line<'_>> {
    let (&prefix, rest) = line.as_bytes().split_first()?;
    if !matches!(prefix, b'+' | b'-' | b' ') {
        return None;
    }
    let leading = rest.iter().take_while(|&&byte| byte == b' ').count();
    let digits = rest[leading..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    let delim = if digits == 0 && leading != 0 {
        leading - 1
    } else {
        leading + digits
    };
    if rest.get(delim) != Some(&b' ') {
        return None;
    }
    Some(Line {
        prefix,
        number: &line[1..1 + delim],
        text: &line[2 + delim..],
    })
}

fn tabs(text: &str) -> Cow<'_, str> {
    if text.contains('\t') {
        Cow::Owned(text.replace('\t', "   "))
    } else {
        Cow::Borrowed(text)
    }
}

fn pair(old: &str, new: &str) -> (String, String) {
    let indent = old
        .chars()
        .zip(new.chars())
        .take_while(|(left, right)| left == right && left.is_whitespace())
        .map(|(c, _)| c.len_utf8())
        .sum();
    let mut removed = old[..indent].to_owned();
    let mut added = removed.clone();
    let mut first_removed = true;
    let mut first_added = true;
    let diff = TextDiff::from_words(&old[indent..], &new[indent..]);
    for change in diff.iter_all_changes() {
        let value = change.value();
        let (line, first) = match change.tag() {
            ChangeTag::Delete => (&mut removed, &mut first_removed),
            ChangeTag::Insert => (&mut added, &mut first_added),
            ChangeTag::Equal => {
                removed.push_str(value);
                added.push_str(value);
                continue;
            }
        };
        let value = if std::mem::take(first) {
            let trimmed = value.trim_start();
            line.push_str(&value[..value.len() - trimmed.len()]);
            trimmed
        } else {
            value
        };
        if !value.is_empty() {
            let _ = write!(line, "\x1b[7m{value}\x1b[27m");
        }
    }
    (removed, added)
}

pub fn render_diff(diff: &str, theme: &Theme, _options: RenderDiffOptions) -> String {
    let mut output = String::with_capacity(diff.len());
    let mut preceding_removal = false;
    let mut first = true;
    let mut lines = diff.split('\n').peekable();
    while let Some(line) = lines.next() {
        if !std::mem::take(&mut first) {
            output.push('\n');
        }
        let Some(parsed) = parse(line) else {
            output.push_str(&theme.fg("toolDiffContext", &tabs(line)));
            preceding_removal = false;
            continue;
        };
        let token = match parsed.prefix {
            b'-' => "toolDiffRemoved",
            b'+' => "toolDiffAdded",
            _ => "toolDiffContext",
        };
        if parsed.prefix == b'-'
            && !preceding_removal
            && let Some(next) = lines.peek().and_then(|line| parse(line))
            && next.prefix == b'+'
        {
            lines.next();
            if !lines
                .peek()
                .and_then(|line| parse(line))
                .is_some_and(|line| line.prefix == b'+')
            {
                let (old, new) = pair(&tabs(parsed.text), &tabs(next.text));
                output.push_str(&theme.fg(token, &format!("-{} {old}", parsed.number)));
                output.push('\n');
                output.push_str(&theme.fg("toolDiffAdded", &format!("+{} {new}", next.number)));
                continue;
            }
            output.push_str(&theme.fg(token, &format!("-{} {}", parsed.number, tabs(parsed.text))));
            output.push('\n');
            output.push_str(&theme.fg(
                "toolDiffAdded",
                &format!("+{} {}", next.number, tabs(next.text)),
            ));
            continue;
        }
        preceding_removal = parsed.prefix == b'-';
        output.push_str(&theme.fg(
            token,
            &format!(
                "{}{} {}",
                char::from(parsed.prefix),
                parsed.number,
                tabs(parsed.text)
            ),
        ));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_numbers_and_only_inverts_changed_tokens() -> Result<(), &'static str> {
        let line = parse("-  42    let old = 3;").ok_or("numbered diff should parse")?;
        assert_eq!(line.number, "  42");
        assert_eq!(line.text, "   let old = 3;");
        let (old, new) = pair(line.text, "   let new = 3;");
        assert_eq!(old, "   let \x1b[7mold\x1b[27m = 3;");
        assert_eq!(new, "   let \x1b[7mnew\x1b[27m = 3;");
        assert!(parse("@@ -1 +1 @@").is_none());
        Ok(())
    }
}
