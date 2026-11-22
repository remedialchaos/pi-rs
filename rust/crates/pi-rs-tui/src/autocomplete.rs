//! Slash command, path, and attachment completion.

use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use icu_collator::{Collator, CollatorBorrowed};

use crate::fuzzy::fuzzy_filter;
use crate::utils::{is_autocomplete_separator, is_whitespace_char};

static COLLATOR: LazyLock<CollatorBorrowed<'static>> = LazyLock::new(|| {
    // INFALLIBLE: The default root locale is included in the compiled ICU data.
    Collator::try_new(Default::default(), Default::default()).expect("compiled root collator")
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutocompleteItem {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

pub type CompleteArguments = dyn Fn(&str) -> Option<Vec<AutocompleteItem>> + Send + Sync;

pub struct SlashCommand {
    pub name: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    pub get_argument_completions: Option<Box<CompleteArguments>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutocompleteSuggestions {
    pub items: Vec<AutocompleteItem>,
    pub prefix: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedCompletion {
    pub lines: Vec<String>,
    pub cursor_line: usize,
    pub cursor_col: usize,
}

#[derive(Default, Clone, Copy)]
pub struct AutocompleteOptions<'a> {
    pub signal: Option<&'a AtomicBool>,
    pub force: bool,
}

impl AutocompleteOptions<'_> {
    pub fn aborted(self) -> bool {
        self.signal
            .is_some_and(|signal| signal.load(Ordering::Acquire))
    }
}

pub trait AutocompleteProvider: Send + Sync {
    fn trigger_characters(&self) -> &[char] {
        &[]
    }

    fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        options: AutocompleteOptions<'_>,
    ) -> Option<AutocompleteSuggestions>;

    fn apply_completion(
        &self,
        lines: Vec<String>,
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> AppliedCompletion;

    fn should_trigger_file_completion(
        &self,
        _lines: &[String],
        _cursor_line: usize,
        _cursor_col: usize,
    ) -> bool {
        true
    }
}

pub struct CombinedAutocompleteProvider {
    commands: Vec<SlashCommand>,
    base_path: PathBuf,
    fd_path: Option<PathBuf>,
}

impl CombinedAutocompleteProvider {
    pub fn new(commands: Vec<SlashCommand>, base_path: PathBuf, fd_path: Option<PathBuf>) -> Self {
        Self {
            commands,
            base_path,
            fd_path,
        }
    }

    fn direct_paths(&self, prefix: PathPrefix<'_>) -> Vec<AutocompleteItem> {
        let PathPrefix { raw, .. } = prefix;
        let expanded = expand_home(raw);
        let dir_prefix = raw.ends_with('/') || matches!(raw, "" | "~");
        let (directory, search_prefix) = if dir_prefix {
            (expanded.as_path(), "")
        } else {
            let name = expanded.file_name().and_then(|s| s.to_str()).unwrap_or("");
            (expanded.parent().unwrap_or(Path::new("")), name)
        };
        let directory = if raw.starts_with('~') || directory.is_absolute() {
            directory.to_path_buf()
        } else {
            self.base_path.join(directory)
        };
        let Ok(entries) = fs::read_dir(directory) else {
            return Vec::new();
        };
        let lower_prefix = search_prefix.to_lowercase();
        let mut suggestions: Vec<_> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !name.to_lowercase().starts_with(&lower_prefix) {
                    return None;
                }
                let is_dir = entry.file_type().ok().is_some_and(|kind| {
                    kind.is_dir() || (kind.is_symlink() && entry.path().is_dir())
                });
                let mut path = if raw.ends_with('/') {
                    format!("{raw}{name}")
                } else if let Some((dir, _)) = raw.rsplit_once('/') {
                    format!("{dir}/{name}")
                } else if raw.starts_with('~') {
                    format!("~/{name}")
                } else {
                    name.to_string()
                }
                .replace('\\', "/");
                let mut label = name.into_owned();
                if is_dir {
                    path.push('/');
                    label.push('/');
                }
                Some(AutocompleteItem {
                    value: completion_value(&path, prefix),
                    label,
                    description: None,
                })
            })
            .collect();
        suggestions.sort_by(|a, b| {
            b.label
                .ends_with('/')
                .cmp(&a.label.ends_with('/'))
                .then_with(|| COLLATOR.compare(&a.label, &b.label))
        });
        suggestions
    }

    fn fuzzy_paths(
        &self,
        prefix: PathPrefix<'_>,
        options: AutocompleteOptions<'_>,
    ) -> Vec<AutocompleteItem> {
        let Some(fd) = self.fd_path.as_deref() else {
            return Vec::new();
        };
        let normalized = prefix.raw.replace('\\', "/");
        let scoped = normalized.rsplit_once('/').and_then(|(dir, query)| {
            let display = &normalized[..dir.len() + 1];
            let base = self.base_path.join(expand_home(display));
            base.is_dir().then_some((base, query, display))
        });
        let (base, query, display) = scoped.as_ref().map_or(
            (self.base_path.as_path(), prefix.raw, ""),
            |(base, query, display)| (base.as_path(), *query, *display),
        );
        let direct = fd_paths(fd, base, query, true, options);
        let recursive = fd_paths(fd, base, query, false, options);
        if options.aborted() {
            return Vec::new();
        }
        let lower_query = query.to_lowercase();
        let mut seen = HashSet::new();
        let mut paths: Vec<_> = direct
            .iter()
            .chain(&recursive)
            .filter_map(|path| {
                if !seen.insert(path.as_str()) {
                    return None;
                }
                let bare = path.trim_end_matches('/');
                let name = bare.rsplit('/').next()?;
                let is_dir = path.ends_with('/');
                let lower_name = name.to_lowercase();
                let score = if query.is_empty() {
                    1
                } else {
                    let score = if lower_name == lower_query {
                        100
                    } else if lower_name.starts_with(&lower_query) {
                        80
                    } else if lower_name.contains(&lower_query) {
                        50
                    } else if path.to_lowercase().contains(&lower_query) {
                        30
                    } else {
                        return None;
                    };
                    score + if is_dir { 10 } else { 0 }
                };
                let depth = bare.split('/').filter(|part| !part.is_empty()).count();
                Some((
                    score,
                    depth,
                    path.encode_utf16().count(),
                    path,
                    name,
                    bare,
                    is_dir,
                ))
            })
            .collect();
        paths.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then(a.1.cmp(&b.1))
                .then(a.2.cmp(&b.2))
                .then_with(|| COLLATOR.compare(a.3, b.3))
        });
        paths
            .into_iter()
            .take(20)
            .map(|(_, _, _, path, name, bare, is_dir)| AutocompleteItem {
                value: completion_value(&format!("{display}{path}"), prefix),
                label: if is_dir {
                    format!("{name}/")
                } else {
                    name.to_owned()
                },
                description: Some(format!("{display}{bare}")),
            })
            .collect()
    }
}

impl AutocompleteProvider for CombinedAutocompleteProvider {
    fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        options: AutocompleteOptions<'_>,
    ) -> Option<AutocompleteSuggestions> {
        let (_, text, _) = split_line(lines, cursor_line, cursor_col);
        let quoted = quoted_prefix(text);
        let token = strip_wrappers(
            text.rmatch_indices(is_delimiter)
                .next()
                .map_or(text, |(i, delimiter)| &text[i + delimiter.len()..]),
        );
        let at = quoted
            .filter(|t| t.starts_with("@\""))
            .or_else(|| token.starts_with('@').then_some(token));
        let (prefix, items) = if let Some(at) = at {
            (at, self.fuzzy_paths(PathPrefix::new(at), options))
        } else if let Some(command_text) = text
            .trim_start_matches(is_whitespace_char)
            .strip_prefix('/')
            .filter(|_| !options.force)
        {
            if let Some((command_name, argument)) = command_text.split_once(' ') {
                let command = self
                    .commands
                    .iter()
                    .find(|command| command.name == command_name)?;
                (
                    argument,
                    command.get_argument_completions.as_ref()?(argument)?,
                )
            } else {
                let indices: Vec<_> = (0..self.commands.len()).collect();
                let primary = fuzzy_filter(&indices, command_text, |i| {
                    self.commands[*i]
                        .name
                        .strip_prefix("skill:")
                        .unwrap_or(&self.commands[*i].name)
                });
                let secondary: Vec<_> = indices
                    .into_iter()
                    .filter(|i| {
                        self.commands[*i].name.starts_with("skill:") && !primary.contains(i)
                    })
                    .collect();
                let secondary = fuzzy_filter(&secondary, command_text, |i| {
                    self.commands[*i].name.as_str()
                });
                let items = primary
                    .into_iter()
                    .chain(secondary)
                    .map(|i| {
                        let command = &self.commands[i];
                        let hint = command.argument_hint.as_deref().filter(|s| !s.is_empty());
                        let description = command.description.as_deref().filter(|s| !s.is_empty());
                        let description = match (hint, description) {
                            (Some(hint), Some(description)) => {
                                Some(format!("{hint} — {description}"))
                            }
                            (Some(s), None) | (None, Some(s)) => Some(s.to_owned()),
                            (None, None) => None,
                        };
                        AutocompleteItem {
                            value: command.name.clone(),
                            label: command.name.clone(),
                            description,
                        }
                    })
                    .collect();
                (text.trim_start_matches(is_whitespace_char), items)
            }
        } else {
            let path = quoted.or_else(|| {
                (options.force
                    || token.contains('/')
                    || token.starts_with('.')
                    || token.is_empty() && text.ends_with(is_autocomplete_separator))
                .then_some(token)
            })?;
            (path, self.direct_paths(PathPrefix::new(path)))
        };
        (!items.is_empty()).then(|| AutocompleteSuggestions {
            items,
            prefix: prefix.to_owned(),
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
        let (_, before_cursor, after) = split_line(&lines, cursor_line, cursor_col);
        let before = before_cursor.strip_suffix(prefix).unwrap_or(before_cursor);
        let after = if PathPrefix::new(prefix).quoted && item.value.ends_with('"') {
            after.strip_prefix('"').unwrap_or(after)
        } else {
            after
        };
        let slash = prefix.strip_prefix('/').is_some_and(|name| {
            !name.contains('/') && before.trim_matches(is_whitespace_char).is_empty()
        });
        let is_dir = item.label.ends_with('/');
        let suffix = if slash || prefix.starts_with('@') && !is_dir {
            " "
        } else {
            ""
        };
        let command_prefix = if slash { "/" } else { "" };
        let cursor_col = before.len() + command_prefix.len() + item.value.len() + suffix.len()
            - usize::from(!slash && is_dir && item.value.ends_with('"'));
        let line = format!("{before}{command_prefix}{}{suffix}{after}", item.value);
        if cursor_line >= lines.len() {
            lines.resize_with(cursor_line + 1, String::new);
        }
        lines[cursor_line] = line;
        AppliedCompletion {
            lines,
            cursor_line,
            cursor_col,
        }
    }

    fn should_trigger_file_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
    ) -> bool {
        let (_, text, _) = split_line(lines, cursor_line, cursor_col);
        let text = text.trim_matches(is_whitespace_char);
        !text.starts_with('/') || text.contains(' ')
    }
}

fn split_line(lines: &[String], line: usize, col: usize) -> (&str, &str, &str) {
    let text = lines.get(line).map_or("", String::as_str);
    let mut col = col.min(text.len());
    while !text.is_char_boundary(col) {
        col -= 1;
    }
    let (left, right) = text.split_at(col);
    (text, left, right)
}

#[derive(Clone, Copy)]
struct PathPrefix<'a> {
    raw: &'a str,
    at: bool,
    quoted: bool,
}

impl<'a> PathPrefix<'a> {
    fn new(prefix: &'a str) -> Self {
        let (raw, at) = prefix
            .strip_prefix('@')
            .map_or((prefix, false), |s| (s, true));
        let (raw, quoted) = raw.strip_prefix('"').map_or((raw, false), |s| (s, true));
        Self { raw, at, quoted }
    }
}

fn completion_value(path: &str, prefix: PathPrefix<'_>) -> String {
    let at = if prefix.at { "@" } else { "" };
    let quote = if prefix.quoted || path.chars().any(is_autocomplete_separator) {
        "\""
    } else {
        ""
    };
    format!("{at}{quote}{path}{quote}")
}

fn is_delimiter(c: char) -> bool {
    matches!(c, ' ' | '\t' | '"' | '\'' | '=') || is_autocomplete_separator(c)
}

fn wrapper(c: char) -> Option<char> {
    Some(match c {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        '<' => '>',
        '`' => '`',
        _ => return None,
    })
}

fn strip_wrappers(mut token: &str) -> &str {
    while let Some((open, rest)) = token
        .chars()
        .next()
        .map(|open| (open, &token[open.len_utf8()..]))
    {
        if wrapper(open).is_none_or(|close| rest.contains(close)) {
            break;
        }
        token = rest;
    }
    token
}

fn quoted_prefix(text: &str) -> Option<&str> {
    let mut quote = None;
    for (i, _) in text.match_indices('"') {
        quote = if quote.is_some() { None } else { Some(i) };
    }
    let quote = quote?;
    let start = if text[..quote].ends_with('@') {
        quote - 1
    } else {
        quote
    };
    let before = text[..start].trim_end_matches(|c| wrapper(c).is_some());
    (before.is_empty() || before.ends_with(is_delimiter)).then_some(&text[start..])
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path
        .strip_prefix("~/")
        .or_else(|| (path == "~").then_some(""))
        && let Some(home) = std::env::home_dir()
    {
        return home.join(rest.trim_start_matches('/'));
    }
    PathBuf::from(path)
}

fn fd_paths(
    fd: &Path,
    dir: &Path,
    query: &str,
    direct: bool,
    options: AutocompleteOptions<'_>,
) -> Vec<String> {
    if options.aborted() {
        return Vec::new();
    }
    let mut command = Command::new(fd);
    command
        .arg("--base-directory")
        .arg(dir)
        .args([
            "--max-results",
            "100",
            "--type",
            "f",
            "--type",
            "d",
            "--follow",
            "--hidden",
            "--exclude",
            ".git",
            "--exclude",
            ".git/*",
            "--exclude",
            ".git/**",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if direct {
        command.args(["--max-depth", "1"]);
    }
    let normalized = query.replace('\\', "/");
    if normalized.contains('/') {
        command.arg("--full-path");
    }
    if !query.is_empty() {
        let separator = r"[\\/]";
        let mut parts = normalized
            .split('/')
            .filter(|part| !part.is_empty())
            .map(regex::escape);
        let pattern = if let Some(first) = parts.next() {
            let mut pattern = first;
            for part in parts {
                pattern.push_str(separator);
                pattern.push_str(&part);
            }
            if normalized.ends_with('/') {
                pattern.push_str(separator);
            }
            pattern
        } else {
            normalized
        };
        command.arg(pattern);
    }
    let Ok(mut child) = command.spawn() else {
        return Vec::new();
    };
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Vec::new();
    };
    let result = std::thread::scope(|scope| {
        let reader = scope.spawn(move || {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes).ok()?;
            Some(bytes)
        });
        let success = loop {
            if options.aborted() {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
            match child.try_wait() {
                Ok(Some(status)) => break status.success(),
                Ok(None) => std::thread::park_timeout(Duration::from_millis(5)),
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break false;
                }
            }
        };
        success.then(|| reader.join().ok().flatten()).flatten()
    });
    let Some(bytes) = result else {
        return Vec::new();
    };
    String::from_utf8_lossy(&bytes)
        .trim()
        .split('\n')
        .filter(|line| !line.is_empty())
        .map(|line| line.replace('\\', "/"))
        .filter(|line| !line.split('/').any(|part| part == ".git"))
        .collect()
}
