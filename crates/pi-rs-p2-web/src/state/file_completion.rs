//! `@` and Tab file completion in the composer (upstream `file-completion.tsx` +
//! `runtime/completion.ts`). Pure token extraction, matching, insertion and popover key
//! handling; filesystem suggestions come from the host's `FileIndex`.

/// A completion suggestion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionItem {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

impl CompletionItem {
    fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            description: None,
        }
    }
}

/// A directory or file entry the host's index returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name_or_path: &'static str,
    pub is_dir: bool,
}

/// Whether `at` is the whole agent mention token (never completed as a file).
pub fn is_mention_token(at: &str, name: Option<&str>) -> bool {
    match name {
        Some(name) => at.eq_ignore_ascii_case(&format!("@{name}")),
        None => false,
    }
}

/// The mention offered first while chat mode is on, when the typed prefix starts its name.
pub fn mention_items(raw_prefix: &str, name: Option<&str>) -> Vec<CompletionItem> {
    let Some(name) = name else {
        return vec![];
    };
    let token = name.to_ascii_lowercase();
    if token.starts_with(&raw_prefix.to_ascii_lowercase()) {
        vec![CompletionItem {
            value: format!("@{token}"),
            label: format!("@{token}"),
            description: Some(format!("mention {name}")),
        }]
    } else {
        vec![]
    }
}

/// Path delimiters that end a token.
const PATH_DELIMITERS: &[char] = &[' ', '\t', '"', '\'', '='];
/// Opening wrappers preceding a path, mapped to their closing counterpart.
const PATH_WRAPPERS: &[(char, char)] =
    &[('(', ')'), ('[', ']'), ('{', '}'), ('<', '>'), ('`', '`')];

fn is_separator(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '，' | '．'
                | '：'
                | '；'
                | '！'
                | '？'
                | '（'
                | '）'
                | '［'
                | '］'
                | '｛'
                | '｝'
                | '“'
                | '”'
                | '‘'
                | '’'
                | '…'
                | '—'
        )
}

fn last_delimiter(text: &str) -> usize {
    // Returns a byte index (upstream returns a UTF-16 index; callers host-bridge ASCII paths).
    let mut last = 0usize;
    for (index, ch) in text.char_indices() {
        if PATH_DELIMITERS.contains(&ch) || is_separator(ch) {
            last = index + ch.len_utf8();
        }
    }
    last
}

fn strip_leading_wrappers(token: &str) -> &str {
    let mut result = token;
    while let Some(ch) = result.chars().next() {
        let Some(closer) = PATH_WRAPPERS
            .iter()
            .find(|(open, _)| *open == ch)
            .map(|(_, close)| *close)
        else {
            break;
        };
        if result[1..].contains(closer) {
            break;
        }
        result = &result[ch.len_utf8()..];
    }
    result
}

fn is_token_start(text: &str, index: usize) -> bool {
    let mut start = index;
    while start > 0 {
        let ch = text[..start].chars().next_back();
        match ch.and_then(|c| PATH_WRAPPERS.iter().find(|(open, _)| *open == c)) {
            Some(_) => {
                start -= text[..start]
                    .chars()
                    .next_back()
                    .map(char::len_utf8)
                    .unwrap_or(1)
            }
            None => break,
        }
    }
    let before = if start == 0 {
        None
    } else {
        text[..start].chars().next_back()
    };
    before
        .map(|c| PATH_DELIMITERS.contains(&c) || is_separator(c))
        .unwrap_or(true)
}

fn extract_quoted_prefix(text: &str) -> Option<String> {
    let mut in_quotes = false;
    let mut start = 0usize;
    for (i, ch) in text.char_indices() {
        if ch == '"' {
            in_quotes = !in_quotes;
            if in_quotes {
                start = i;
            }
        }
    }
    if !in_quotes {
        return None;
    }
    if start > 0 && text[..start].ends_with('@') {
        let at = start - 1;
        if !is_token_start(text, at) {
            return None;
        }
        return Some(text[at..].to_owned());
    }
    if !is_token_start(text, start) {
        return None;
    }
    Some(text[start..].to_owned())
}

/// `(raw prefix, is at prefix, is quoted prefix)`.
pub fn parse_path_prefix(prefix: &str) -> (String, bool, bool) {
    if let Some(rest) = prefix.strip_prefix("@\"") {
        (rest.to_owned(), true, true)
    } else if let Some(rest) = prefix.strip_prefix('"') {
        (rest.to_owned(), false, true)
    } else if let Some(rest) = prefix.strip_prefix('@') {
        (rest.to_owned(), true, false)
    } else {
        (prefix.to_owned(), false, false)
    }
}

/// The `@` token before the cursor (`@src/ma`, `@"my file"`), or `None`.
pub fn extract_at_prefix(text: &str) -> Option<String> {
    if let Some(quoted) = extract_quoted_prefix(text)
        && quoted.starts_with("@\"")
    {
        return Some(quoted);
    }
    let token = strip_leading_wrappers(&text[last_delimiter(text)..]);
    token.starts_with('@').then(|| token.to_owned())
}

/// A path-like token before the cursor; with `force` (Tab) any token, else path-looking ones.
pub fn extract_path_prefix(text: &str, force: bool) -> Option<String> {
    if let Some(quoted) = extract_quoted_prefix(text) {
        return Some(quoted);
    }
    let token = strip_leading_wrappers(&text[last_delimiter(text)..]);
    if force {
        return Some(token.to_owned());
    }
    if token.contains('/') || token.starts_with('.') || token.starts_with("~/") {
        return Some(token.to_owned());
    }
    if token.is_empty() && !text.is_empty() && is_token_start(text, text.len()) {
        return Some(String::new());
    }
    None
}

/// Forced Tab completes files unless a slash-command name is being typed.
pub fn should_trigger_file_completion(before_cursor: &str) -> bool {
    let trimmed = before_cursor.trim();
    !(trimmed.starts_with('/') && !trimmed.contains(' '))
}

/// Paths that leave the session directory complete nothing (cwd-relative only).
pub fn out_of_scope(raw_prefix: &str) -> bool {
    raw_prefix.starts_with('/')
        || raw_prefix.starts_with('~')
        || raw_prefix == ".."
        || raw_prefix.starts_with("../")
}

/// The value inserted for a matched path.
pub fn build_completion_value(
    path: &str,
    _is_directory: bool,
    is_at_prefix: bool,
    is_quoted_prefix: bool,
) -> String {
    let needs_quotes = is_quoted_prefix || path.chars().any(is_separator);
    let prefix = if is_at_prefix { "@" } else { "" };
    if !needs_quotes {
        format!("{prefix}{path}")
    } else {
        format!("{prefix}\"{path}\"")
    }
}

/// Fuzzy file suggestions for `@` completion: the name is the label, the path the description.
pub fn at_items(results: &[(String, bool)], is_quoted_prefix: bool) -> Vec<CompletionItem> {
    results
        .iter()
        .map(|(path, dir)| {
            let name = path.rsplit('/').next().unwrap_or(path.as_str());
            let full = if *dir {
                format!("{path}/")
            } else {
                path.clone()
            };
            CompletionItem {
                value: build_completion_value(&full, *dir, true, is_quoted_prefix),
                label: format!("{name}{}", if *dir { "/" } else { "" }),
                description: Some(path.clone()),
            }
        })
        .collect()
}

/// The directory and name a Tab path completion lists.
pub fn path_completion(raw_prefix: &str) -> (String, String, String) {
    match raw_prefix.rfind('/') {
        Some(slash) => (
            raw_prefix[..slash]
                .trim_start_matches("./")
                .trim_end_matches('/')
                .to_owned(),
            raw_prefix[slash + 1..].to_owned(),
            raw_prefix[..slash + 1].to_owned(),
        ),
        None => {
            let display = if raw_prefix.starts_with("./") {
                "./"
            } else {
                ""
            };
            (String::new(), raw_prefix.to_owned(), display.to_owned())
        }
    }
}

/// Tab path completion over one directory's entries: directories first, then names.
pub fn path_items(
    entries: &[(String, bool)],
    raw_prefix: &str,
    is_at_prefix: bool,
    is_quoted_prefix: bool,
) -> Vec<CompletionItem> {
    let (_, name, display) = path_completion(raw_prefix);
    let lower = name.to_ascii_lowercase();
    let mut matching: Vec<&(String, bool)> = entries
        .iter()
        .filter(|(entry_name, _)| entry_name.to_ascii_lowercase().starts_with(&lower))
        .collect();
    matching.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    matching
        .into_iter()
        .map(|(entry_name, dir)| {
            let full = format!("{display}{entry_name}{}", if *dir { "/" } else { "" });
            CompletionItem::new(
                build_completion_value(&full, *dir, is_at_prefix, is_quoted_prefix),
                format!("{entry_name}{}", if *dir { "/" } else { "" }),
            )
        })
        .collect()
}

/// Inserts a completion item in place of `prefix` before the cursor (upstream `applyFileCompletion`).
pub fn apply_file_completion(
    line: &str,
    cursor_col: usize,
    item: &CompletionItem,
    prefix: &str,
) -> (String, usize) {
    let before_end = cursor_col.saturating_sub(prefix.len()).min(line.len());
    let before = &line[..before_end];
    let after = &line[cursor_col.min(line.len())..];
    let quoted = prefix.starts_with('"') || prefix.starts_with("@\"");
    let rest = if quoted && item.value.ends_with('"') && after.starts_with('"') {
        &after[1..]
    } else {
        after
    };
    let is_directory = item.label.ends_with('/');
    let suffix = if prefix.starts_with('@') && !is_directory {
        " "
    } else {
        ""
    };
    let offset = if is_directory && item.value.ends_with('"') {
        item.value.len() - 1
    } else {
        item.value.len()
    };
    (
        format!("{before}{}{suffix}{rest}", item.value),
        before.len() + offset + suffix.len(),
    )
}

/// The popover shown above the input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Popover {
    pub items: Vec<CompletionItem>,
    pub prefix: String,
    pub selected: usize,
    pub hint: Option<String>,
}

/// The key the popover consumes, if any (`upstream useFileCompletion.onKeyDown`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopoverKey {
    ArrowDown,
    ArrowUp,
    /// Apply the selected item.
    Enter,
    Tab,
    Escape,
}

impl Popover {
    /// Applies a key to the popover, returning `true` when it was consumed.
    pub fn on_key(&mut self, key: PopoverKey, shift: bool) -> bool {
        let count = self.items.len();
        match key {
            PopoverKey::ArrowDown if count > 0 => {
                self.selected = (self.selected + 1) % count;
                true
            }
            PopoverKey::ArrowUp if count > 0 => {
                self.selected = (self.selected + count - 1) % count;
                true
            }
            PopoverKey::Enter | PopoverKey::Tab if !shift && count > 0 => true,
            PopoverKey::Escape => true,
            _ => false,
        }
    }

    /// The currently selected item.
    pub fn selected(&self) -> Option<&CompletionItem> {
        self.items.get(self.selected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_prefix_extraction_and_parsing() {
        assert_eq!(extract_at_prefix("hello @src/ma"), Some("@src/ma".into()));
        assert_eq!(extract_at_prefix("plain text"), None);
        // Upstream slices after `@"`; the closing quote stays in the raw prefix.
        assert_eq!(
            parse_path_prefix("@\"my file\""),
            ("my file\"".into(), true, true)
        );
        assert_eq!(parse_path_prefix("@src"), ("src".into(), true, false));
    }

    #[test]
    fn mention_tokens_are_never_files() {
        assert!(is_mention_token("@pi", Some("Pi")));
        assert!(!is_mention_token("@pizza", Some("Pi")));
        let items = mention_items("p", Some("Pi"));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].value, "@pi");
        assert!(mention_items("z", Some("Pi")).is_empty());
    }

    #[test]
    fn out_of_scope_prefixes() {
        assert!(out_of_scope("/etc/passwd"));
        assert!(out_of_scope("~/notes"));
        assert!(out_of_scope("../../secret"));
        assert!(!out_of_scope("src/main.rs"));
    }

    #[test]
    fn path_completion_and_items_sort_directories_first() {
        assert_eq!(
            path_completion("src/ma"),
            ("src".into(), "ma".into(), "src/".into())
        );
        let entries = vec![
            ("main.rs".to_owned(), false),
            ("mods".to_owned(), true),
            ("make.sh".to_owned(), false),
        ];
        let items = path_items(&entries, "src/ma", false, false);
        // "mods" does not start with "ma"; directories sort first among matches.
        assert_eq!(
            items
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>(),
            vec!["main.rs", "make.sh"]
        );
        let all = path_items(&entries, "src/", false, false);
        assert_eq!(all[0].label, "mods/");
    }

    #[test]
    fn apply_completion_replaces_prefix_and_adds_space_for_files() {
        let item = CompletionItem::new("@src/main.rs", "main.rs");
        let (line, cursor) = apply_file_completion("see @src/ma now", 11, &item, "@src/ma");
        assert_eq!(line, "see @src/main.rs  now");
        assert_eq!(cursor, 17);
    }

    #[test]
    fn popover_key_navigation_wraps_and_escapes() {
        let mut popover = Popover {
            items: vec![CompletionItem::new("a", "a"), CompletionItem::new("b", "b")],
            prefix: "@".into(),
            selected: 0,
            hint: None,
        };
        assert!(popover.on_key(PopoverKey::ArrowUp, false));
        assert_eq!(popover.selected, 1);
        assert!(popover.on_key(PopoverKey::ArrowDown, false));
        assert_eq!(popover.selected, 0);
        assert!(popover.on_key(PopoverKey::Escape, false));
        assert_eq!(
            popover.selected().map(|item| item.value.as_str()),
            Some("a")
        );
    }
}
