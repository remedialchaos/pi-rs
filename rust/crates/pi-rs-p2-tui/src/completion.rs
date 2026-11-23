use pi_rs_tui::autocomplete::{
    AppliedCompletion, AutocompleteItem, AutocompleteOptions, AutocompleteProvider,
    AutocompleteSuggestions,
};
use std::sync::Arc;
use tokio::sync::Mutex;

pub trait FileSource: Send + Sync {
    fn complete(&self, query: &str, limit: usize) -> (Vec<AutocompleteItem>, bool);
    fn list(&self, path: &str) -> Vec<FileEntry>;
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub dir: bool,
}

pub const OUT_OF_SCOPE_HINT: &str = "File completion covers the session directory only";
pub const PARTIAL_HINT: &str = "File index is partial (the directory is very large)";

pub struct FileCompletionProvider {
    slash_provider: Arc<dyn AutocompleteProvider>,
    files: Arc<Mutex<Option<Arc<dyn FileSource>>>>,
    hint: Arc<dyn Fn(String) + Send + Sync>,
    mentions: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    last_kind: Arc<Mutex<CompletionKind>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionKind {
    Slash,
    File,
}

impl FileCompletionProvider {
    pub fn new(
        slash_provider: Arc<dyn AutocompleteProvider>,
        files: Arc<Mutex<Option<Arc<dyn FileSource>>>>,
        hint: impl Fn(String) + Send + Sync + 'static,
        mentions: impl Fn() -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            slash_provider,
            files,
            hint: Arc::new(hint),
            mentions: Arc::new(mentions),
            last_kind: Arc::new(Mutex::new(CompletionKind::File)),
        }
    }
}

impl AutocompleteProvider for FileCompletionProvider {
    fn trigger_characters(&self) -> &[char] {
        static TRIGGERS: &[char] = &['@'];
        TRIGGERS
    }

    fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        options: AutocompleteOptions<'_>,
    ) -> Option<AutocompleteSuggestions> {
        let line = lines.get(cursor_line).map(|s| s.as_str()).unwrap_or("");
        let end = line.floor_char_boundary(cursor_col.min(line.len()));
        let prefix = &line[..end];

        if options.aborted() {
            return None;
        }
        let token = if prefix.matches('"').count() % 2 == 1 {
            let quote = prefix.rfind('"')?;
            let start = if prefix[..quote].ends_with('@') {
                quote - 1
            } else {
                quote
            };
            &prefix[start..]
        } else {
            prefix
                .rsplit(|c: char| c.is_whitespace() || matches!(c, '=' | '\'' | '"'))
                .next()
                .unwrap_or_default()
                .trim_start_matches(['(', '[', '{', '<', '`'])
        };
        let at = token.starts_with('@');
        let query = token.trim_start_matches('@').trim_start_matches('"');
        if !at
            && prefix.trim_start().starts_with('/')
            && (!options.force || prefix.trim_start().contains(char::is_whitespace))
        {
            let slash = self.slash_provider.get_suggestions(
                lines,
                cursor_line,
                cursor_col,
                AutocompleteOptions {
                    force: false,
                    ..options
                },
            );
            if !options.force || slash.as_ref().is_some_and(|s| !s.items.is_empty()) {
                *self.last_kind.try_lock().ok()? = CompletionKind::Slash;
                return slash;
            }
        }
        if !at
            && (!options.force
                || (prefix.trim().starts_with('/') && !prefix.trim().contains(char::is_whitespace)))
        {
            return None;
        }
        *self.last_kind.try_lock().ok()? = CompletionKind::File;
        let mut items = Vec::new();
        if at {
            let mention = (self.mentions)();
            if query.eq_ignore_ascii_case(mention.as_deref().unwrap_or("Pi")) {
                return None;
            }
            if let Some(name) = mention
                && name.to_lowercase().starts_with(&query.to_lowercase())
            {
                let value = format!("@{}", name.to_lowercase());
                items.push(AutocompleteItem {
                    label: value.clone(),
                    value,
                    description: Some(format!("mention {name}")),
                });
            }
        }
        if query.starts_with(['~', '/']) || query.split(['/', '\\']).any(|s| s == "..") {
            (self.hint)(OUT_OF_SCOPE_HINT.into());
        } else if let Some(source) = self.files.try_lock().ok()?.as_ref() {
            if at {
                let (found, truncated) = source.complete(query, 20);
                if truncated {
                    (self.hint)(PARTIAL_HINT.into())
                }
                items.extend(found);
            } else {
                let (dir, name) = query
                    .rsplit_once('/')
                    .map_or(("", query), |(dir, name)| (&query[..dir.len() + 1], name));
                let mut entries = source.list(dir.trim_start_matches("./").trim_end_matches('/'));
                entries.retain(|entry| entry.name.to_lowercase().starts_with(&name.to_lowercase()));
                entries.sort_by(|a, b| b.dir.cmp(&a.dir).then(a.name.cmp(&b.name)));
                for entry in entries {
                    let label = format!("{}{}", entry.name, if entry.dir { "/" } else { "" });
                    let path = format!("{dir}{label}");
                    let value = if token.starts_with('"') || path.contains(char::is_whitespace) {
                        format!("\"{path}\"")
                    } else {
                        path
                    };
                    items.push(AutocompleteItem {
                        value,
                        label,
                        description: None,
                    });
                }
            }
        }
        (!options.aborted() && !items.is_empty()).then(|| AutocompleteSuggestions {
            items,
            prefix: token.to_owned(),
        })
    }

    fn apply_completion(
        &self,
        mut lines: Vec<String>,
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> AppliedCompletion {
        if self
            .last_kind
            .try_lock()
            .is_ok_and(|kind| *kind == CompletionKind::Slash)
        {
            return self.slash_provider.apply_completion(
                lines,
                cursor_line,
                cursor_col,
                item,
                prefix,
            );
        }
        let Some(line) = lines.get_mut(cursor_line) else {
            return AppliedCompletion {
                lines,
                cursor_line,
                cursor_col,
            };
        };
        let end = line.floor_char_boundary(cursor_col.min(line.len()));
        let start = line[..end].strip_suffix(prefix).map_or(end, str::len);
        let dir = item.label.ends_with('/');
        let suffix = if prefix.starts_with('@') && !dir {
            " "
        } else {
            ""
        };
        let quoted = item.value.ends_with('"');
        let new_cursor_col = start + item.value.len() + suffix.len() - usize::from(dir && quoted);
        let drop_quote = quoted
            && line[end..].starts_with('"')
            && (prefix.starts_with('"') || prefix.starts_with("@\""));
        line.replace_range(
            start..end + usize::from(drop_quote),
            &format!("{}{suffix}", item.value),
        );
        AppliedCompletion {
            lines,
            cursor_line,
            cursor_col: new_cursor_col,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AtItems {
    pub items: Vec<AutocompleteItem>,
}

impl AtItems {
    pub fn new(items: Vec<AutocompleteItem>) -> Self {
        Self { items }
    }
}
