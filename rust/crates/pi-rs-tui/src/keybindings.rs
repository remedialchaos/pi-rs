//! The ordered TUI binding registry and user overrides.

use std::sync::{Arc, LazyLock, RwLock};

use indexmap::IndexMap;

use crate::keys::{KeyId, matches_key_data};

pub type Keybinding = String;
pub type KeybindingDefinitions = IndexMap<Keybinding, KeybindingDefinition>;
pub type KeybindingsConfig = IndexMap<Keybinding, BindingKeys>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingKeys {
    One(KeyId),
    Many(Vec<KeyId>),
}

impl BindingKeys {
    fn as_slice(&self) -> &[KeyId] {
        match self {
            Self::One(key) => std::slice::from_ref(key),
            Self::Many(keys) => keys,
        }
    }

    fn dedup(&mut self) {
        if let Self::Many(keys) = self {
            let mut i = 0;
            while i < keys.len() {
                if keys[..i].contains(&keys[i]) {
                    keys.remove(i);
                } else {
                    i += 1;
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingDefinition {
    pub default_keys: BindingKeys,
    pub description: Option<String>,
}

pub const TUI_KEYBINDINGS: &[(&str, &[&str], &str)] = &[
    ("tui.editor.cursorUp", &["up"], "Move cursor up"),
    ("tui.editor.cursorDown", &["down"], "Move cursor down"),
    (
        "tui.editor.historyPrevious",
        &[],
        "Select previous prompt history entry",
    ),
    (
        "tui.editor.historyNext",
        &[],
        "Select next prompt history entry",
    ),
    (
        "tui.editor.cursorLeft",
        &["left", "ctrl+b"],
        "Move cursor left",
    ),
    (
        "tui.editor.cursorRight",
        &["right", "ctrl+f"],
        "Move cursor right",
    ),
    (
        "tui.editor.cursorWordLeft",
        &["alt+left", "ctrl+left", "alt+b"],
        "Move cursor word left",
    ),
    (
        "tui.editor.cursorWordRight",
        &["alt+right", "ctrl+right", "alt+f"],
        "Move cursor word right",
    ),
    (
        "tui.editor.cursorLineStart",
        &["home", "ctrl+a"],
        "Move to line start",
    ),
    (
        "tui.editor.cursorLineEnd",
        &["end", "ctrl+e"],
        "Move to line end",
    ),
    (
        "tui.editor.jumpForward",
        &["ctrl+]"],
        "Jump forward to character",
    ),
    (
        "tui.editor.jumpBackward",
        &["ctrl+alt+]"],
        "Jump backward to character",
    ),
    ("tui.editor.pageUp", &["pageUp", "ctrl+pageUp"], "Page up"),
    (
        "tui.editor.pageDown",
        &["pageDown", "ctrl+pageDown"],
        "Page down",
    ),
    (
        "tui.editor.deleteCharBackward",
        &["backspace"],
        "Delete character backward",
    ),
    (
        "tui.editor.deleteCharForward",
        &["delete", "ctrl+d"],
        "Delete character forward",
    ),
    (
        "tui.editor.deleteWordBackward",
        &["ctrl+w", "alt+backspace"],
        "Delete word backward",
    ),
    (
        "tui.editor.deleteWordForward",
        &["alt+d", "alt+delete"],
        "Delete word forward",
    ),
    (
        "tui.editor.deleteToLineStart",
        &["ctrl+u"],
        "Delete to line start",
    ),
    (
        "tui.editor.deleteToLineEnd",
        &["ctrl+k"],
        "Delete to line end",
    ),
    ("tui.editor.yank", &["ctrl+y"], "Yank"),
    ("tui.editor.yankPop", &["alt+y"], "Yank pop"),
    ("tui.editor.undo", &["ctrl+-"], "Undo"),
    (
        "tui.input.newLine",
        &["shift+enter", "ctrl+j"],
        "Insert newline",
    ),
    ("tui.input.submit", &["enter"], "Submit input"),
    ("tui.input.tab", &["tab"], "Tab / autocomplete"),
    ("tui.input.copy", &["ctrl+c"], "Copy selection"),
    ("tui.select.up", &["up"], "Move selection up"),
    ("tui.select.down", &["down"], "Move selection down"),
    ("tui.select.pageUp", &["pageUp"], "Selection page up"),
    ("tui.select.pageDown", &["pageDown"], "Selection page down"),
    ("tui.select.confirm", &["enter"], "Confirm selection"),
    (
        "tui.select.cancel",
        &["escape", "ctrl+c"],
        "Cancel selection",
    ),
    (
        "tui.altScreen.pageUp",
        &["pageUp"],
        "Scroll viewport up one page",
    ),
    (
        "tui.altScreen.pageDown",
        &["pageDown"],
        "Scroll viewport down one page",
    ),
    (
        "tui.altScreen.halfPageUp",
        &[],
        "Scroll viewport up half a page",
    ),
    (
        "tui.altScreen.halfPageDown",
        &[],
        "Scroll viewport down half a page",
    ),
    ("tui.altScreen.lineUp", &[], "Scroll viewport up one line"),
    (
        "tui.altScreen.lineDown",
        &[],
        "Scroll viewport down one line",
    ),
    (
        "tui.altScreen.previousPrompt",
        &["ctrl+shift+up", "ctrl+up"],
        "Jump to previous semantic prompt",
    ),
    (
        "tui.altScreen.nextPrompt",
        &["ctrl+shift+down", "ctrl+down"],
        "Jump to next semantic prompt",
    ),
    (
        "tui.altScreen.search",
        &["ctrl+shift+f"],
        "Search the primary scroll view",
    ),
    (
        "tui.altScreen.searchNext",
        &["enter", "ctrl+g"],
        "Select the next search match",
    ),
    (
        "tui.altScreen.searchPrevious",
        &["shift+enter", "ctrl+shift+g"],
        "Select the previous search match",
    ),
    (
        "tui.altScreen.searchClose",
        &["escape"],
        "Close transcript search",
    ),
    (
        "tui.altScreen.top",
        &["ctrl+home"],
        "Scroll viewport to top",
    ),
    (
        "tui.altScreen.bottom",
        &["ctrl+end"],
        "Scroll viewport to bottom",
    ),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingConflict {
    pub key: KeyId,
    pub keybindings: Vec<Keybinding>,
}

#[derive(Debug, Clone)]
pub struct KeybindingsManager {
    definitions: KeybindingDefinitions,
    user_bindings: KeybindingsConfig,
    keys_by_id: IndexMap<Keybinding, Vec<KeyId>>,
    conflicts: Vec<KeybindingConflict>,
}

impl Default for KeybindingsManager {
    fn default() -> Self {
        let definitions = TUI_KEYBINDINGS
            .iter()
            .map(|(id, keys, description)| {
                let default_keys = if let [key] = *keys {
                    BindingKeys::One((*key).to_owned())
                } else {
                    BindingKeys::Many(keys.iter().map(|k| (*k).to_owned()).collect())
                };
                (
                    (*id).to_owned(),
                    KeybindingDefinition {
                        default_keys,
                        description: Some((*description).to_owned()),
                    },
                )
            })
            .collect();
        Self::new(definitions, KeybindingsConfig::new())
    }
}

impl KeybindingsManager {
    pub fn new(definitions: KeybindingDefinitions, user_bindings: KeybindingsConfig) -> Self {
        let mut manager = Self {
            definitions,
            user_bindings: KeybindingsConfig::new(),
            keys_by_id: IndexMap::new(),
            conflicts: Vec::new(),
        };
        manager.set_user_bindings(user_bindings);
        manager
    }

    pub fn matches(&self, data: &str, keybinding: &str) -> bool {
        self.get_keys(keybinding)
            .iter()
            .any(|key| matches_key_data(data, key))
    }

    pub fn get_keys(&self, keybinding: &str) -> &[KeyId] {
        self.keys_by_id.get(keybinding).map_or(&[], Vec::as_slice)
    }

    pub fn get_definition(&self, keybinding: &str) -> Option<&KeybindingDefinition> {
        self.definitions.get(keybinding)
    }

    pub fn get_conflicts(&self) -> &[KeybindingConflict] {
        &self.conflicts
    }

    pub fn set_user_bindings(&mut self, user_bindings: KeybindingsConfig) {
        self.user_bindings = user_bindings;
        self.keys_by_id.clear();
        self.conflicts.clear();
        let mut claims: IndexMap<&str, Vec<&str>> = IndexMap::new();
        for (id, keys) in &self.user_bindings {
            if !self.definitions.contains_key(id) {
                continue;
            }
            for key in keys.as_slice() {
                let claim = claims.entry(key).or_default();
                if claim.last().copied() != Some(id.as_str()) {
                    claim.push(id);
                }
            }
        }
        self.conflicts
            .extend(
                claims
                    .into_iter()
                    .filter(|(_, ids)| ids.len() > 1)
                    .map(|(key, ids)| KeybindingConflict {
                        key: key.to_owned(),
                        keybindings: ids.into_iter().map(str::to_owned).collect(),
                    }),
            );
        for (id, definition) in &self.definitions {
            let mut binding = self
                .user_bindings
                .get(id)
                .unwrap_or(&definition.default_keys)
                .clone();
            binding.dedup();
            let keys = match binding {
                BindingKeys::One(key) => vec![key],
                BindingKeys::Many(keys) => keys,
            };
            self.keys_by_id.insert(id.clone(), keys);
        }
    }

    pub fn get_user_bindings(&self) -> &KeybindingsConfig {
        &self.user_bindings
    }

    pub fn get_resolved_bindings(&self) -> KeybindingsConfig {
        self.keys_by_id
            .iter()
            .map(|(id, keys)| {
                let binding = if let [key] = keys.as_slice() {
                    BindingKeys::One(key.clone())
                } else {
                    BindingKeys::Many(keys.clone())
                };
                (id.clone(), binding)
            })
            .collect()
    }
}

static GLOBAL: LazyLock<RwLock<Arc<KeybindingsManager>>> =
    LazyLock::new(|| RwLock::new(Arc::new(KeybindingsManager::default())));

pub fn get_keybindings() -> Arc<KeybindingsManager> {
    Arc::clone(
        &GLOBAL
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

pub fn set_keybindings(manager: KeybindingsManager) {
    *GLOBAL
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(manager);
}

pub fn get_select_page_index(
    bindings: &KeybindingsManager,
    data: &str,
    selected: usize,
    count: usize,
    page_size: usize,
) -> Option<usize> {
    let next = if bindings.matches(data, "tui.select.pageUp") {
        selected.saturating_sub(page_size)
    } else if bindings.matches(data, "tui.select.pageDown") {
        selected.saturating_add(page_size)
    } else {
        return None;
    };
    Some(next.min(count.saturating_sub(1)))
}
