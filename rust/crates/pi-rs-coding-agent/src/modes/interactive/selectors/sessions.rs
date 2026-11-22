use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Instant;

use chrono::Utc;
use fancy_regex::Regex;
use pi_rs_tui::{Component, Focusable, fuzzy_match, get_keybindings};

use super::{InputDialog, Picker};
use crate::core::session_manager::SessionInfo;
use crate::core::skills::{canonicalize_path, path_comparison_key};
use crate::modes::interactive::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionSort {
    #[default]
    Threaded,
    Recent,
    Relevance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionNameFilter {
    #[default]
    All,
    Named,
}

fn normalize(text: &str) -> String {
    text.split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

enum Token {
    Fuzzy(String),
    Phrase(String),
}

enum Query {
    Tokens(Vec<Token>),
    Pattern(Regex),
}

impl Query {
    fn parse(query: &str) -> Result<Self, String> {
        let query = query.trim();
        if let Some(pattern) = query.strip_prefix("re:") {
            let pattern = pattern.trim();
            if pattern.is_empty() {
                return Err("Empty regex".into());
            }
            return Regex::new(&format!("(?i){pattern}"))
                .map(Self::Pattern)
                .map_err(|err| err.to_string());
        }
        if !query.matches('"').count().is_multiple_of(2) {
            return Ok(Self::Tokens(
                query
                    .split_whitespace()
                    .map(|text| Token::Fuzzy(text.to_lowercase()))
                    .collect(),
            ));
        }
        let mut tokens = Vec::new();
        for (index, part) in query.split('"').enumerate() {
            if index % 2 == 0 {
                tokens.extend(
                    part.split_whitespace()
                        .map(|text| Token::Fuzzy(text.to_lowercase())),
                );
            } else if !part.trim().is_empty() {
                tokens.push(Token::Phrase(normalize(part)));
            }
        }
        Ok(Self::Tokens(tokens))
    }
}

#[derive(Default)]
struct SearchText {
    name: OnceCell<String>,
    id: OnceCell<String>,
    cwd: OnceCell<String>,
    messages: OnceCell<String>,
    phrase_name: OnceCell<String>,
    phrase_cwd: OnceCell<String>,
    phrase_messages: OnceCell<String>,
}

impl SearchText {
    fn score(&self, session: &SessionInfo, query: &Query) -> Option<f64> {
        const TEXT: f64 = 1_000_000.0;
        const FUZZY: f64 = 2_000_000.0;
        let name = session.name.as_deref().unwrap_or_default();
        match query {
            Query::Pattern(pattern) => {
                let matches = |field| pattern.is_match(field).is_ok_and(|matches| matches);
                if !name.is_empty() && matches(name) {
                    Some(0.0)
                } else {
                    [&session.id, &session.all_messages_text, &session.cwd]
                        .iter()
                        .any(|text| matches(text))
                        .then_some(TEXT)
                }
            }
            Query::Tokens(tokens) => tokens.iter().try_fold(0.0, |score, token| {
                let id = self.id.get_or_init(|| session.id.to_lowercase());
                let value = match token {
                    Token::Phrase(value) => {
                        let name = self.phrase_name.get_or_init(|| normalize(name));
                        if name.contains(value) {
                            fuzzy_match(value, name).score
                        } else if id.contains(value)
                            || self
                                .phrase_messages
                                .get_or_init(|| normalize(&session.all_messages_text))
                                .contains(value)
                            || self
                                .phrase_cwd
                                .get_or_init(|| normalize(&session.cwd))
                                .contains(value)
                        {
                            TEXT
                        } else {
                            return None;
                        }
                    }
                    Token::Fuzzy(value) => {
                        let name = self.name.get_or_init(|| name.to_lowercase());
                        let cwd = self.cwd.get_or_init(|| session.cwd.to_lowercase());
                        if name.contains(value) {
                            fuzzy_match(value, name).score
                        } else if id.contains(value)
                            || self
                                .messages
                                .get_or_init(|| session.all_messages_text.to_lowercase())
                                .contains(value)
                            || cwd.contains(value)
                        {
                            TEXT
                        } else {
                            let best = [name, cwd]
                                .into_iter()
                                .map(|text| fuzzy_match(value, text))
                                .filter(|candidate| candidate.matches)
                                .map(|candidate| candidate.score)
                                .min_by(f64::total_cmp)?;
                            FUZZY + best
                        }
                    }
                };
                Some(score + value)
            }),
        }
    }
}

struct IndexedSession {
    info: SessionInfo,
    searchable: SearchText,
    path_key: String,
    parent_key: Option<String>,
}

impl IndexedSession {
    fn new(info: SessionInfo) -> Self {
        let path_key = session_path_key(&info.path);
        let parent_key = info.parent_session_path.as_deref().map(session_path_key);
        Self {
            info,
            searchable: SearchText::default(),
            path_key,
            parent_key,
        }
    }
}

fn session_path_key(path: &Path) -> String {
    path_comparison_key(&canonicalize_path(path))
}

fn has_session_name(session: &SessionInfo) -> bool {
    session
        .name
        .as_deref()
        .is_some_and(|name| !name.trim().is_empty())
}

fn rank(
    entries: &[IndexedSession],
    query: &str,
    sort: SessionSort,
    names: SessionNameFilter,
) -> Result<Vec<usize>, String> {
    let indices = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| names == SessionNameFilter::All || has_session_name(&entry.info))
        .map(|(index, _)| index);
    if query.trim().is_empty() {
        return Ok(if sort == SessionSort::Threaded {
            threaded(entries, indices)
                .into_iter()
                .map(|(index, _)| index)
                .collect()
        } else {
            indices.collect()
        });
    }
    let query = Query::parse(query)?;
    let mut scored: Vec<_> = indices
        .filter_map(|index| {
            let entry = &entries[index];
            entry
                .searchable
                .score(&entry.info, &query)
                .map(|score| (index, score))
        })
        .collect();
    if sort != SessionSort::Recent {
        scored.sort_by(|&(a, a_score), &(b, b_score)| {
            a_score.total_cmp(&b_score).then_with(|| {
                entries[b]
                    .info
                    .modified_millis
                    .cmp(&entries[a].info.modified_millis)
            })
        });
    }
    Ok(scored.into_iter().map(|(index, _)| index).collect())
}

/// Match by original index. Empty threaded searches group forks under their parent.
/// Other searches use relevance unless Recent is selected.
pub fn filter_and_sort_sessions(
    sessions: &[SessionInfo],
    query: &str,
    sort: SessionSort,
    names: SessionNameFilter,
) -> Result<Vec<usize>, String> {
    if query.trim().is_empty() && sort == SessionSort::Threaded {
        let entries: Vec<_> = sessions.iter().cloned().map(IndexedSession::new).collect();
        return rank(&entries, query, sort, names);
    }
    let query = Query::parse(query)?;
    let mut scored: Vec<_> = sessions
        .iter()
        .enumerate()
        .filter(|(_, session)| names == SessionNameFilter::All || has_session_name(session))
        .filter_map(|(index, session)| {
            SearchText::default()
                .score(session, &query)
                .map(|score| (index, score))
        })
        .collect();
    if !matches!(&query, Query::Tokens(tokens) if tokens.is_empty()) && sort != SessionSort::Recent
    {
        scored.sort_by(|&(a, a_score), &(b, b_score)| {
            a_score.total_cmp(&b_score).then_with(|| {
                sessions[b]
                    .modified_millis
                    .cmp(&sessions[a].modified_millis)
            })
        });
    }
    Ok(scored.into_iter().map(|(index, _)| index).collect())
}

/// Iterative postorder max activity and then preorder rendering. Malformed parent cycles
/// are detached once, rather than hiding all sessions in the cycle or recursing forever.
fn threaded(
    entries: &[IndexedSession],
    indices: impl Iterator<Item = usize>,
) -> Vec<(usize, String)> {
    let indices: Vec<_> = indices.collect();
    let by_path: HashMap<_, _> = indices
        .iter()
        .map(|&index| (entries[index].path_key.as_str(), index))
        .collect();
    let mut children = vec![Vec::new(); entries.len()];
    let mut roots = Vec::new();
    for &index in &indices {
        match entries[index]
            .parent_key
            .as_deref()
            .and_then(|path| by_path.get(path).copied())
            .filter(|parent| *parent != index)
        {
            Some(parent) => children[parent].push(index),
            None => roots.push(index),
        }
    }
    let mut timestamps: Vec<_> = entries
        .iter()
        .map(|entry| entry.info.modified_millis)
        .collect();
    let mut seen = vec![false; entries.len()];
    let mut stack = Vec::new();
    let mut walk = Vec::with_capacity(indices.len());
    let root_candidates: Vec<_> = roots
        .iter()
        .copied()
        .chain(indices.iter().copied())
        .collect();
    for root in root_candidates {
        if seen[root] {
            continue;
        }
        if !roots.contains(&root) {
            roots.push(root);
        }
        stack.push(root);
        let start = walk.len();
        while let Some(index) = stack.pop() {
            if std::mem::replace(&mut seen[index], true) {
                continue;
            }
            walk.push(index);
            children[index].retain(|&child| !seen[child]);
            stack.extend(children[index].iter().copied());
        }
        for &index in walk[start..].iter().rev() {
            for &child in &children[index] {
                timestamps[index] = timestamps[index].max(timestamps[child]);
            }
            children[index].sort_by_key(|&child| std::cmp::Reverse(timestamps[child]));
        }
    }
    roots.sort_by_key(|&root| std::cmp::Reverse(timestamps[root]));
    let mut stack: Vec<_> = roots
        .into_iter()
        .rev()
        .map(|root| (root, String::new(), None))
        .collect();
    let mut ordered = Vec::with_capacity(indices.len());
    while let Some((index, base, sibling)) = stack.pop() {
        let mut prefix = base.clone();
        if let Some(is_last) = sibling {
            prefix.push_str(if is_last { "└─ " } else { "├─ " });
        }
        ordered.push((index, prefix));
        let mut base = base;
        if let Some(is_last) = sibling {
            base.push_str(if is_last { "   " } else { "│  " });
        }
        for (pos, &child) in children[index].iter().enumerate().rev() {
            stack.push((child, base.clone(), Some(pos + 1 == children[index].len())));
        }
    }
    ordered
}

fn format_age(now: i64, timestamp: i64) -> String {
    let minutes = now.saturating_sub(timestamp) / 60_000;
    let hours = minutes / 60;
    let days = hours / 24;
    match (minutes, hours, days) {
        (..=0, _, _) => "now".into(),
        (..=59, _, _) => format!("{minutes}m"),
        (_, ..=23, _) => format!("{hours}h"),
        (_, _, ..=6) => format!("{days}d"),
        (_, _, ..=29) => format!("{}w", days / 7),
        (_, _, ..=364) => format!("{}mo", days / 30),
        _ => format!("{}y", days / 365),
    }
}

type Sessions = Rc<RefCell<Vec<IndexedSession>>>;
type LoadAll = Box<dyn FnMut() -> Result<Vec<SessionInfo>, String> + Send>;
type Rename = Box<dyn FnMut(&Path, &str) -> Result<(), String>>;
type SessionSelection = Box<dyn FnMut(Option<&SessionInfo>)>;
type Loading = Receiver<(LoadAll, Result<Vec<SessionInfo>, String>)>;

pub struct SessionPicker {
    picker: Picker,
    theme: Theme,
    current: Sessions,
    all: Option<Sessions>,
    loading: Option<Loading>,
    load_all: Option<LoadAll>,
    sort: SessionSort,
    names: SessionNameFilter,
    show_all: bool,
    show_path: bool,
    current_path: Option<String>,
    deletion: Option<PathBuf>,
    rename: Option<Rename>,
    rename_input: Option<(PathBuf, InputDialog)>,
    rename_result: Rc<RefCell<Option<Option<String>>>>,
    status: String,
    select: Rc<RefCell<SessionSelection>>,
}

impl SessionPicker {
    pub fn new(
        current: Vec<SessionInfo>,
        load_all: impl FnMut() -> Result<Vec<SessionInfo>, String> + Send + 'static,
        theme: Theme,
        select: impl FnMut(Option<&SessionInfo>) + 'static,
    ) -> Self {
        let mut dialog = Self {
            picker: Picker::new(String::new(), Vec::new(), theme.clone(), |_| {}).without_wrap(),
            current: Rc::new(RefCell::new(
                current.into_iter().map(IndexedSession::new).collect(),
            )),
            all: None,
            loading: None,
            load_all: Some(Box::new(load_all)),
            sort: SessionSort::Threaded,
            names: SessionNameFilter::All,
            show_all: false,
            show_path: false,
            current_path: None,
            deletion: None,
            rename: None,
            rename_input: None,
            rename_result: Rc::new(RefCell::new(None)),
            theme,
            status: String::new(),
            select: Rc::new(RefCell::new(Box::new(select))),
        };
        dialog.refresh(None);
        dialog
    }

    pub fn with_current_path(mut self, path: Option<&Path>) -> Self {
        self.current_path = path.map(session_path_key);
        self.refresh(None);
        self
    }

    pub fn with_rename(
        mut self,
        rename: impl FnMut(&Path, &str) -> Result<(), String> + 'static,
    ) -> Self {
        self.rename = Some(Box::new(rename));
        self.refresh(None);
        self
    }

    fn rows(&self) -> Option<&Sessions> {
        if self.show_all {
            self.all.as_ref()
        } else {
            Some(&self.current)
        }
    }

    fn selected(&self) -> Option<(PathBuf, Option<String>)> {
        let index = self.picker.selected_index()?;
        let entries = self.rows()?.borrow();
        let info = &entries.get(index)?.info;
        Some((info.path.clone(), info.name.clone()))
    }

    fn refresh(&mut self, keep: Option<&Path>) {
        let rows = self.rows().cloned().unwrap_or_default();
        let entries = rows.borrow();
        let paths = self.show_path;
        let all = self.show_all;
        let sort = self.sort;
        let names = self.names;
        let keep = keep.and_then(|path| entries.iter().position(|entry| entry.info.path == path));
        let display_thread = sort == SessionSort::Threaded && self.picker.query().trim().is_empty();
        let now = Utc::now().timestamp_millis();
        let mut labels: Vec<_> = entries
            .iter()
            .map(|entry| {
                let info = &entry.info;
                let text = info
                    .name
                    .as_deref()
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or(&info.first_message);
                let text: String = text
                    .chars()
                    .map(|ch| if ch.is_control() { ' ' } else { ch })
                    .collect();
                let marker = if self.current_path.as_ref() == Some(&entry.path_key) {
                    "✓ "
                } else {
                    ""
                };
                let mut row = format!(
                    "{marker}{}  ·  {} {}",
                    text.trim(),
                    info.message_count,
                    format_age(now, info.modified_millis)
                );
                if all {
                    row.push_str(&format!("  {}", info.cwd));
                }
                if paths {
                    row.push_str(&format!("  {}", info.path.display()));
                }
                row
            })
            .collect();
        if display_thread {
            for (index, prefix) in threaded(
                &entries,
                (0..entries.len()).filter(|&index| {
                    names == SessionNameFilter::All || has_session_name(&entries[index].info)
                }),
            ) {
                labels[index].insert_str(0, &prefix);
            }
        }
        drop(entries);
        let ranked = Rc::clone(&rows);
        self.picker.replace_rows(
            labels,
            Box::new(move |query| rank(&ranked.borrow(), query, sort, names).unwrap_or_default()),
            keep,
        );
        let select = Rc::clone(&self.select);
        self.picker.select = Box::new(move |index| {
            let rows = rows.borrow();
            select.borrow_mut()(
                index
                    .and_then(|index| rows.get(index))
                    .map(|entry| &entry.info),
            );
        });
        self.picker.title = if all {
            "Resume Session (All)"
        } else {
            "Resume Session (Current Folder)"
        }
        .into();
        self.picker.hint = format!(
            "Tab scope · Ctrl+S sort · Ctrl+N named · Ctrl+D delete · Ctrl+P path{} · Esc cancel",
            if self.rename.is_some() {
                " · Ctrl+R rename"
            } else {
                ""
            }
        );
        self.update_status();
    }

    fn update_status(&mut self) {
        let sort = match self.sort {
            SessionSort::Threaded => "Threaded",
            SessionSort::Recent => "Recent",
            SessionSort::Relevance => "Fuzzy",
        };
        let names = if self.names == SessionNameFilter::Named {
            "Named"
        } else {
            "All"
        };
        let status = if self.show_all && self.loading.is_some() {
            "Loading sessions…"
        } else {
            &self.status
        };
        let regex_error = Query::parse(self.picker.query()).err();
        let status = regex_error.as_deref().unwrap_or(status);
        self.picker.description =
            format!("Sort: {sort} · Name: {names} · re:<pattern> regex · \"phrase\" exact");
        if !status.is_empty() {
            self.picker.description.push('\n');
            self.picker.description.push_str(status);
        }
        if self.deletion.is_some() {
            self.picker
                .description
                .push_str("\nDelete selected session? Enter confirm · Esc cancel");
        }
    }

    fn start_all(&mut self) {
        if self.all.is_some() || self.loading.is_some() {
            return;
        }
        let Some(mut loader) = self.load_all.take() else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let sessions = loader();
            let _ = tx.send((loader, sessions));
        });
        self.loading = Some(rx);
    }

    fn delete(&mut self, path: &Path) {
        use std::process::{Command, Stdio};
        let trashed = Command::new("trash")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if trashed || !path.exists() {
            self.status = "Session moved to trash".into();
        } else if let Err(err) = std::fs::remove_file(path) {
            self.status = format!("Failed to delete: {err}");
            self.update_status();
            return;
        } else {
            self.status = "Session deleted".into();
        }
        self.current
            .borrow_mut()
            .retain(|entry| entry.info.path != path);
        if let Some(all) = &self.all {
            all.borrow_mut().retain(|entry| entry.info.path != path);
        }
        self.refresh(None);
    }
}

impl Component for SessionPicker {
    fn render(&self, width: usize) -> Vec<String> {
        match &self.rename_input {
            Some((_, input)) => input.render(width),
            None => self.picker.render(width),
        }
    }

    fn handle_input(&mut self, data: &str) {
        if let Some((_, input)) = &mut self.rename_input {
            input.handle_input(data);
            let result = self.rename_result.borrow_mut().take();
            if let Some(result) = result
                && let Some((path, _)) = self.rename_input.take()
            {
                if let Some(value) = result {
                    let name = value.trim();
                    if !name.is_empty()
                        && let Some(rename) = &mut self.rename
                    {
                        match rename(&path, name) {
                            Ok(()) => {
                                for rows in std::iter::once(&self.current).chain(self.all.iter()) {
                                    if let Some(entry) = rows
                                        .borrow_mut()
                                        .iter_mut()
                                        .find(|entry| entry.info.path == path)
                                    {
                                        entry.info.name = Some(name.to_owned());
                                        entry.searchable = SearchText::default();
                                    }
                                }
                            }
                            Err(error) => self.status = format!("Failed to rename: {error}"),
                        }
                    }
                }
                self.refresh(Some(&path));
            }
            return;
        }
        let keys = get_keybindings();
        let is = |name: &str| keys.matches(data, name);
        if self.deletion.is_some() {
            if is("tui.select.confirm")
                && let Some(path) = self.deletion.take()
            {
                self.delete(&path);
            } else if is("tui.select.cancel") {
                self.deletion = None;
                self.update_status();
            }
            return;
        }
        if is("tui.input.tab") {
            self.show_all = !self.show_all;
            if self.show_all {
                self.start_all();
            }
            self.refresh(None);
        } else if is("app.session.toggleSort") {
            self.sort = match self.sort {
                SessionSort::Threaded => SessionSort::Recent,
                SessionSort::Recent => SessionSort::Relevance,
                SessionSort::Relevance => SessionSort::Threaded,
            };
            self.refresh(None);
        } else if is("app.session.toggleNamedFilter") {
            self.names = if self.names == SessionNameFilter::Named {
                SessionNameFilter::All
            } else {
                SessionNameFilter::Named
            };
            self.refresh(None);
        } else if is("app.session.togglePath") {
            self.show_path = !self.show_path;
            let keep = self.selected().map(|(path, _)| path);
            self.refresh(keep.as_deref());
        } else if is("app.session.delete")
            || (is("app.session.deleteNoninvasive") && self.picker.query().is_empty())
        {
            if let Some((path, _)) = self.selected() {
                if self.current_path.as_ref() == Some(&session_path_key(&path)) {
                    self.status = "Cannot delete the currently active session".into();
                } else {
                    self.deletion = Some(path);
                }
                self.update_status();
            }
        } else if is("app.session.rename") && self.rename.is_some() {
            if let Some((path, name)) = self.selected() {
                let result = Rc::clone(&self.rename_result);
                let mut input = InputDialog::new(
                    "Rename Session".into(),
                    None,
                    self.theme.clone(),
                    move |name| {
                        *result.borrow_mut() = Some(name);
                    },
                )
                .initial_value(name.unwrap_or_default());
                input.set_focused(self.picker.is_focused());
                self.rename_input = Some((path, input));
            }
        } else {
            let previous = self.picker.query().to_owned();
            self.picker.handle_input(data);
            if previous != self.picker.query() {
                if previous.trim().is_empty() != self.picker.query().trim().is_empty()
                    && self.sort == SessionSort::Threaded
                {
                    let keep = self.selected().map(|(path, _)| path);
                    self.refresh(keep.as_deref());
                } else {
                    self.update_status();
                }
            }
        }
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
    fn captures_viewport_input(&self) -> bool {
        self.rename_input.is_none()
    }

    fn tick(&mut self, _now: Instant) -> bool {
        let response = self.loading.as_ref().map(Receiver::try_recv);
        match response {
            Some(Ok((loader, result))) => {
                self.loading = None;
                self.load_all = Some(loader);
                match result {
                    Ok(sessions) => {
                        self.all = Some(Rc::new(RefCell::new(
                            sessions.into_iter().map(IndexedSession::new).collect(),
                        )))
                    }
                    Err(error) => self.status = format!("Failed to load sessions: {error}"),
                }
                if self.show_all {
                    self.refresh(None);
                }
                true
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.loading = None;
                self.status = "Session loader stopped".into();
                self.update_status();
                true
            }
            Some(Err(TryRecvError::Empty)) | None => false,
        }
    }
}

impl Focusable for SessionPicker {
    fn is_focused(&self) -> bool {
        self.picker.is_focused()
    }
    fn set_focused(&mut self, focused: bool) {
        self.picker.set_focused(focused);
        if let Some((_, input)) = &mut self.rename_input {
            input.set_focused(focused);
        }
    }
}
