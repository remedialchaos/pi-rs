use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyPlatform {
    pub platform: String,
    pub windows_keys: bool,
}

impl Default for KeyPlatform {
    fn default() -> Self {
        Self {
            platform: "linux".to_string(),
            windows_keys: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum KeyIdValue {
    One(String),
    Many(Vec<String>),
}
impl KeyIdValue {
    pub fn as_slice(&self) -> &[String] {
        match self {
            Self::One(s) => std::slice::from_ref(s),
            Self::Many(v) => v,
        }
    }
}
pub type KeybindingsConfig = IndexMap<String, KeyIdValue>;
pub struct KeybindingDefinition {
    pub default_keys: KeyIdValue,
    pub description: &'static str,
}
fn one(key: &'static str) -> KeyIdValue {
    KeyIdValue::One(key.to_string())
}

fn many(keys: &[&'static str]) -> KeyIdValue {
    KeyIdValue::Many(keys.iter().map(|key| (*key).to_string()).collect())
}

fn empty() -> KeyIdValue {
    KeyIdValue::Many(Vec::new())
}

/// Build the full keybinding registry in declaration order.
pub fn builtin_keybindings(defaults: KeyPlatform) -> IndexMap<&'static str, KeybindingDefinition> {
    let mut map = IndexMap::new();

    // TUI_KEYBINDINGS, in upstream declaration order.
    map.insert(
        "tui.editor.cursorUp",
        KeybindingDefinition {
            default_keys: one("up"),
            description: "Move cursor up",
        },
    );
    map.insert(
        "tui.editor.cursorDown",
        KeybindingDefinition {
            default_keys: one("down"),
            description: "Move cursor down",
        },
    );
    map.insert(
        "tui.editor.historyPrevious",
        KeybindingDefinition {
            default_keys: empty(),
            description: "Select previous prompt history entry",
        },
    );
    map.insert(
        "tui.editor.historyNext",
        KeybindingDefinition {
            default_keys: empty(),
            description: "Select next prompt history entry",
        },
    );
    map.insert(
        "tui.editor.cursorLeft",
        KeybindingDefinition {
            default_keys: many(&["left", "ctrl+b"]),
            description: "Move cursor left",
        },
    );
    map.insert(
        "tui.editor.cursorRight",
        KeybindingDefinition {
            default_keys: many(&["right", "ctrl+f"]),
            description: "Move cursor right",
        },
    );
    map.insert(
        "tui.editor.cursorWordLeft",
        KeybindingDefinition {
            default_keys: many(&["alt+left", "ctrl+left", "alt+b"]),
            description: "Move cursor word left",
        },
    );
    map.insert(
        "tui.editor.cursorWordRight",
        KeybindingDefinition {
            default_keys: many(&["alt+right", "ctrl+right", "alt+f"]),
            description: "Move cursor word right",
        },
    );
    map.insert(
        "tui.editor.cursorLineStart",
        KeybindingDefinition {
            default_keys: many(&["home", "ctrl+a"]),
            description: "Move to line start",
        },
    );
    map.insert(
        "tui.editor.cursorLineEnd",
        KeybindingDefinition {
            default_keys: many(&["end", "ctrl+e"]),
            description: "Move to line end",
        },
    );
    map.insert(
        "tui.editor.jumpForward",
        KeybindingDefinition {
            default_keys: one("ctrl+]"),
            description: "Jump forward to character",
        },
    );
    map.insert(
        "tui.editor.jumpBackward",
        KeybindingDefinition {
            default_keys: one("ctrl+alt+]"),
            description: "Jump backward to character",
        },
    );
    map.insert(
        "tui.editor.pageUp",
        KeybindingDefinition {
            default_keys: many(&["pageUp", "ctrl+pageUp"]),
            description: "Page up",
        },
    );
    map.insert(
        "tui.editor.pageDown",
        KeybindingDefinition {
            default_keys: many(&["pageDown", "ctrl+pageDown"]),
            description: "Page down",
        },
    );
    map.insert(
        "tui.editor.deleteCharBackward",
        KeybindingDefinition {
            default_keys: one("backspace"),
            description: "Delete character backward",
        },
    );
    map.insert(
        "tui.editor.deleteCharForward",
        KeybindingDefinition {
            default_keys: many(&["delete", "ctrl+d"]),
            description: "Delete character forward",
        },
    );
    map.insert(
        "tui.editor.deleteWordBackward",
        KeybindingDefinition {
            default_keys: many(&["ctrl+w", "alt+backspace"]),
            description: "Delete word backward",
        },
    );
    map.insert(
        "tui.editor.deleteWordForward",
        KeybindingDefinition {
            default_keys: many(&["alt+d", "alt+delete"]),
            description: "Delete word forward",
        },
    );
    map.insert(
        "tui.editor.deleteToLineStart",
        KeybindingDefinition {
            default_keys: one("ctrl+u"),
            description: "Delete to line start",
        },
    );
    map.insert(
        "tui.editor.deleteToLineEnd",
        KeybindingDefinition {
            default_keys: one("ctrl+k"),
            description: "Delete to line end",
        },
    );
    map.insert(
        "tui.editor.yank",
        KeybindingDefinition {
            default_keys: one("ctrl+y"),
            description: "Yank",
        },
    );
    map.insert(
        "tui.editor.yankPop",
        KeybindingDefinition {
            default_keys: one("alt+y"),
            description: "Yank pop",
        },
    );
    let undo_defaults = if defaults.platform == "win32" {
        one("ctrl+z")
    } else if defaults.windows_keys {
        one("alt+z")
    } else {
        one("ctrl+-")
    };
    map.insert(
        "tui.editor.undo",
        KeybindingDefinition {
            default_keys: undo_defaults,
            description: "Undo",
        },
    );
    map.insert(
        "tui.input.newLine",
        KeybindingDefinition {
            default_keys: many(&["shift+enter", "ctrl+j"]),
            description: "Insert newline",
        },
    );
    map.insert(
        "tui.input.submit",
        KeybindingDefinition {
            default_keys: one("enter"),
            description: "Submit input",
        },
    );
    map.insert(
        "tui.input.tab",
        KeybindingDefinition {
            default_keys: one("tab"),
            description: "Tab / autocomplete",
        },
    );
    map.insert(
        "tui.input.copy",
        KeybindingDefinition {
            default_keys: one("ctrl+c"),
            description: "Copy selection",
        },
    );
    map.insert(
        "tui.select.up",
        KeybindingDefinition {
            default_keys: one("up"),
            description: "Move selection up",
        },
    );
    map.insert(
        "tui.select.down",
        KeybindingDefinition {
            default_keys: one("down"),
            description: "Move selection down",
        },
    );
    map.insert(
        "tui.select.pageUp",
        KeybindingDefinition {
            default_keys: one("pageUp"),
            description: "Selection page up",
        },
    );
    map.insert(
        "tui.select.pageDown",
        KeybindingDefinition {
            default_keys: one("pageDown"),
            description: "Selection page down",
        },
    );
    map.insert(
        "tui.select.confirm",
        KeybindingDefinition {
            default_keys: one("enter"),
            description: "Confirm selection",
        },
    );
    map.insert(
        "tui.select.cancel",
        KeybindingDefinition {
            default_keys: many(&["escape", "ctrl+c"]),
            description: "Cancel selection",
        },
    );
    map.insert(
        "tui.altScreen.pageUp",
        KeybindingDefinition {
            default_keys: one("pageUp"),
            description: "Scroll viewport up one page",
        },
    );
    map.insert(
        "tui.altScreen.pageDown",
        KeybindingDefinition {
            default_keys: one("pageDown"),
            description: "Scroll viewport down one page",
        },
    );
    map.insert(
        "tui.altScreen.halfPageUp",
        KeybindingDefinition {
            default_keys: empty(),
            description: "Scroll viewport up half a page",
        },
    );
    map.insert(
        "tui.altScreen.halfPageDown",
        KeybindingDefinition {
            default_keys: empty(),
            description: "Scroll viewport down half a page",
        },
    );
    map.insert(
        "tui.altScreen.lineUp",
        KeybindingDefinition {
            default_keys: empty(),
            description: "Scroll viewport up one line",
        },
    );
    map.insert(
        "tui.altScreen.lineDown",
        KeybindingDefinition {
            default_keys: empty(),
            description: "Scroll viewport down one line",
        },
    );
    let previous_prompt = if defaults.windows_keys {
        one("ctrl+up")
    } else {
        many(&["ctrl+shift+up", "ctrl+up"])
    };
    map.insert(
        "tui.altScreen.previousPrompt",
        KeybindingDefinition {
            default_keys: previous_prompt,
            description: "Jump to previous semantic prompt",
        },
    );
    let next_prompt = if defaults.windows_keys {
        one("ctrl+down")
    } else {
        many(&["ctrl+shift+down", "ctrl+down"])
    };
    map.insert(
        "tui.altScreen.nextPrompt",
        KeybindingDefinition {
            default_keys: next_prompt,
            description: "Jump to next semantic prompt",
        },
    );
    let search = if defaults.windows_keys {
        one("ctrl+f")
    } else {
        one("ctrl+shift+f")
    };
    map.insert(
        "tui.altScreen.search",
        KeybindingDefinition {
            default_keys: search,
            description: "Search the primary scroll view",
        },
    );
    map.insert(
        "tui.altScreen.searchNext",
        KeybindingDefinition {
            default_keys: many(&["enter", "ctrl+g"]),
            description: "Select the next search match",
        },
    );
    map.insert(
        "tui.altScreen.searchPrevious",
        KeybindingDefinition {
            default_keys: many(&["shift+enter", "ctrl+shift+g"]),
            description: "Select the previous search match",
        },
    );
    map.insert(
        "tui.altScreen.searchClose",
        KeybindingDefinition {
            default_keys: one("escape"),
            description: "Close transcript search",
        },
    );
    map.insert(
        "tui.altScreen.top",
        KeybindingDefinition {
            default_keys: one("ctrl+home"),
            description: "Scroll viewport to top",
        },
    );
    map.insert(
        "tui.altScreen.bottom",
        KeybindingDefinition {
            default_keys: one("ctrl+end"),
            description: "Scroll viewport to bottom",
        },
    );

    // APP_KEYBINDINGS, in upstream declaration order (43 ids).
    map.insert(
        "app.interrupt",
        KeybindingDefinition {
            default_keys: one("escape"),
            description: "Cancel or abort",
        },
    );
    map.insert(
        "app.clear",
        KeybindingDefinition {
            default_keys: one("ctrl+c"),
            description: "Clear editor",
        },
    );
    map.insert(
        "app.exit",
        KeybindingDefinition {
            default_keys: one("ctrl+d"),
            description: "Exit when editor is empty",
        },
    );
    let suspend = if defaults.platform == "win32" {
        empty()
    } else {
        one("ctrl+z")
    };
    map.insert(
        "app.suspend",
        KeybindingDefinition {
            default_keys: suspend,
            description: "Suspend to background",
        },
    );
    map.insert(
        "app.thinking.cycle",
        KeybindingDefinition {
            default_keys: one("shift+tab"),
            description: "Cycle thinking level",
        },
    );
    map.insert(
        "app.thinking.save",
        KeybindingDefinition {
            default_keys: one("ctrl+s"),
            description: "Save thinking level",
        },
    );
    map.insert(
        "app.model.cycleForward",
        KeybindingDefinition {
            default_keys: one("ctrl+p"),
            description: "Cycle to next model",
        },
    );
    let cycle_backward = if defaults.windows_keys {
        one("alt+p")
    } else {
        one("shift+ctrl+p")
    };
    map.insert(
        "app.model.cycleBackward",
        KeybindingDefinition {
            default_keys: cycle_backward,
            description: "Cycle to previous model",
        },
    );
    map.insert(
        "app.model.select",
        KeybindingDefinition {
            default_keys: one("ctrl+l"),
            description: "Open model selector",
        },
    );
    map.insert(
        "app.tools.expand",
        KeybindingDefinition {
            default_keys: one("ctrl+o"),
            description: "Toggle tool output",
        },
    );
    map.insert(
        "app.thinking.toggle",
        KeybindingDefinition {
            default_keys: one("ctrl+t"),
            description: "Toggle thinking blocks",
        },
    );
    map.insert(
        "app.session.toggleNamedFilter",
        KeybindingDefinition {
            default_keys: one("ctrl+n"),
            description: "Toggle named session filter",
        },
    );
    map.insert(
        "app.editor.external",
        KeybindingDefinition {
            default_keys: one("ctrl+g"),
            description: "Open external editor",
        },
    );
    map.insert(
        "app.message.copy",
        KeybindingDefinition {
            default_keys: one("ctrl+x"),
            description: "Copy selection or last assistant message",
        },
    );
    let follow_up = if defaults.windows_keys {
        one("ctrl+q")
    } else {
        one("alt+enter")
    };
    map.insert(
        "app.message.followUp",
        KeybindingDefinition {
            default_keys: follow_up,
            description: "Queue follow-up message",
        },
    );
    let dequeue = if defaults.windows_keys {
        one("alt+q")
    } else {
        one("alt+up")
    };
    map.insert(
        "app.message.dequeue",
        KeybindingDefinition {
            default_keys: dequeue,
            description: "Restore queued messages",
        },
    );
    let paste_image = if defaults.windows_keys {
        many(&["alt+v", "ctrl+v"])
    } else {
        one("ctrl+v")
    };
    map.insert(
        "app.clipboard.pasteImage",
        KeybindingDefinition {
            default_keys: paste_image,
            description: "Paste files on macOS, images, or text from clipboard",
        },
    );
    map.insert(
        "app.session.new",
        KeybindingDefinition {
            default_keys: empty(),
            description: "Start a new session",
        },
    );
    map.insert(
        "app.session.tree",
        KeybindingDefinition {
            default_keys: empty(),
            description: "Open session tree",
        },
    );
    map.insert(
        "app.session.fork",
        KeybindingDefinition {
            default_keys: empty(),
            description: "Fork current session",
        },
    );
    map.insert(
        "app.session.resume",
        KeybindingDefinition {
            default_keys: empty(),
            description: "Resume a session",
        },
    );
    let fold = if defaults.platform == "darwin" {
        many(&["alt+left", "ctrl+left"])
    } else {
        many(&["ctrl+left", "alt+left"])
    };
    map.insert(
        "app.tree.foldOrUp",
        KeybindingDefinition {
            default_keys: fold,
            description: "Fold tree branch or move up",
        },
    );
    let unfold = if defaults.platform == "darwin" {
        many(&["alt+right", "ctrl+right"])
    } else {
        many(&["ctrl+right", "alt+right"])
    };
    map.insert(
        "app.tree.unfoldOrDown",
        KeybindingDefinition {
            default_keys: unfold,
            description: "Unfold tree branch or move down",
        },
    );
    map.insert(
        "app.tree.editLabel",
        KeybindingDefinition {
            default_keys: one("shift+l"),
            description: "Edit tree label",
        },
    );
    map.insert(
        "app.tree.toggleLabelTimestamp",
        KeybindingDefinition {
            default_keys: one("shift+t"),
            description: "Toggle tree label timestamps",
        },
    );
    map.insert(
        "app.session.togglePath",
        KeybindingDefinition {
            default_keys: one("ctrl+p"),
            description: "Toggle session path display",
        },
    );
    map.insert(
        "app.session.toggleSort",
        KeybindingDefinition {
            default_keys: one("ctrl+s"),
            description: "Toggle session sort mode",
        },
    );
    map.insert(
        "app.session.rename",
        KeybindingDefinition {
            default_keys: one("ctrl+r"),
            description: "Rename session",
        },
    );
    map.insert(
        "app.session.delete",
        KeybindingDefinition {
            default_keys: one("ctrl+d"),
            description: "Delete session",
        },
    );
    map.insert(
        "app.session.deleteNoninvasive",
        KeybindingDefinition {
            default_keys: one("ctrl+backspace"),
            description: "Delete session when query is empty",
        },
    );
    map.insert(
        "app.models.save",
        KeybindingDefinition {
            default_keys: one("ctrl+s"),
            description: "Save model selection",
        },
    );
    map.insert(
        "app.models.enableAll",
        KeybindingDefinition {
            default_keys: one("ctrl+a"),
            description: "Enable all models",
        },
    );
    map.insert(
        "app.models.clearAll",
        KeybindingDefinition {
            default_keys: one("ctrl+x"),
            description: "Clear all models",
        },
    );
    map.insert(
        "app.models.toggleProvider",
        KeybindingDefinition {
            default_keys: one("ctrl+p"),
            description: "Toggle all models for provider",
        },
    );
    map.insert(
        "app.models.reorderUp",
        KeybindingDefinition {
            default_keys: one("alt+up"),
            description: "Move model up in order",
        },
    );
    map.insert(
        "app.models.reorderDown",
        KeybindingDefinition {
            default_keys: one("alt+down"),
            description: "Move model down in order",
        },
    );
    map.insert(
        "app.tree.filter.default",
        KeybindingDefinition {
            default_keys: one("ctrl+d"),
            description: "Tree filter: default view",
        },
    );
    map.insert(
        "app.tree.filter.noTools",
        KeybindingDefinition {
            default_keys: one("ctrl+t"),
            description: "Tree filter: hide tool results",
        },
    );
    map.insert(
        "app.tree.filter.userOnly",
        KeybindingDefinition {
            default_keys: one("ctrl+u"),
            description: "Tree filter: user messages only",
        },
    );
    map.insert(
        "app.tree.filter.labeledOnly",
        KeybindingDefinition {
            default_keys: one("ctrl+l"),
            description: "Tree filter: labeled entries only",
        },
    );
    map.insert(
        "app.tree.filter.all",
        KeybindingDefinition {
            default_keys: one("ctrl+a"),
            description: "Tree filter: show all entries",
        },
    );
    map.insert(
        "app.tree.filter.cycleForward",
        KeybindingDefinition {
            default_keys: one("ctrl+o"),
            description: "Tree filter: cycle forward",
        },
    );
    map.insert(
        "app.tree.filter.cycleBackward",
        KeybindingDefinition {
            default_keys: one("shift+ctrl+o"),
            description: "Tree filter: cycle backward",
        },
    );

    map.insert(
        "p2.attach",
        KeybindingDefinition {
            default_keys: empty(),
            description: "Attach a local file (launcher)",
        },
    );
    map
}
fn legacy_map() -> IndexMap<&'static str, &'static str> {
    let mut map = IndexMap::new();
    for (old, new) in [
        ("cursorUp", "tui.editor.cursorUp"),
        ("cursorDown", "tui.editor.cursorDown"),
        ("cursorLeft", "tui.editor.cursorLeft"),
        ("cursorRight", "tui.editor.cursorRight"),
        ("cursorWordLeft", "tui.editor.cursorWordLeft"),
        ("cursorWordRight", "tui.editor.cursorWordRight"),
        ("cursorLineStart", "tui.editor.cursorLineStart"),
        ("cursorLineEnd", "tui.editor.cursorLineEnd"),
        ("jumpForward", "tui.editor.jumpForward"),
        ("jumpBackward", "tui.editor.jumpBackward"),
        ("pageUp", "tui.editor.pageUp"),
        ("pageDown", "tui.editor.pageDown"),
        ("deleteCharBackward", "tui.editor.deleteCharBackward"),
        ("deleteCharForward", "tui.editor.deleteCharForward"),
        ("deleteWordBackward", "tui.editor.deleteWordBackward"),
        ("deleteWordForward", "tui.editor.deleteWordForward"),
        ("deleteToLineStart", "tui.editor.deleteToLineStart"),
        ("deleteToLineEnd", "tui.editor.deleteToLineEnd"),
        ("yank", "tui.editor.yank"),
        ("yankPop", "tui.editor.yankPop"),
        ("undo", "tui.editor.undo"),
        ("newLine", "tui.input.newLine"),
        ("submit", "tui.input.submit"),
        ("tab", "tui.input.tab"),
        ("copy", "tui.input.copy"),
        ("selectUp", "tui.select.up"),
        ("selectDown", "tui.select.down"),
        ("selectPageUp", "tui.select.pageUp"),
        ("selectPageDown", "tui.select.pageDown"),
        ("selectConfirm", "tui.select.confirm"),
        ("selectCancel", "tui.select.cancel"),
        ("interrupt", "app.interrupt"),
        ("clear", "app.clear"),
        ("exit", "app.exit"),
        ("suspend", "app.suspend"),
        ("cycleThinkingLevel", "app.thinking.cycle"),
        ("cycleModelForward", "app.model.cycleForward"),
        ("cycleModelBackward", "app.model.cycleBackward"),
        ("selectModel", "app.model.select"),
        ("expandTools", "app.tools.expand"),
        ("toggleThinking", "app.thinking.toggle"),
        ("toggleSessionNamedFilter", "app.session.toggleNamedFilter"),
        ("externalEditor", "app.editor.external"),
        ("followUp", "app.message.followUp"),
        ("dequeue", "app.message.dequeue"),
        ("pasteImage", "app.clipboard.pasteImage"),
        ("newSession", "app.session.new"),
        ("tree", "app.session.tree"),
        ("fork", "app.session.fork"),
        ("resume", "app.session.resume"),
        ("treeFoldOrUp", "app.tree.foldOrUp"),
        ("treeUnfoldOrDown", "app.tree.unfoldOrDown"),
        ("treeEditLabel", "app.tree.editLabel"),
        ("treeToggleLabelTimestamp", "app.tree.toggleLabelTimestamp"),
        ("toggleSessionPath", "app.session.togglePath"),
        ("toggleSessionSort", "app.session.toggleSort"),
        ("renameSession", "app.session.rename"),
        ("deleteSession", "app.session.delete"),
        ("deleteSessionNoninvasive", "app.session.deleteNoninvasive"),
    ] {
        map.insert(old, new);
    }
    map
}
const BASE_KEYS: &[&str] = &[
    "escape",
    "esc",
    "enter",
    "return",
    "tab",
    "space",
    "backspace",
    "delete",
    "insert",
    "clear",
    "home",
    "end",
    "pageUp",
    "pageDown",
    "up",
    "down",
    "left",
    "right",
    "f1",
    "f2",
    "f3",
    "f4",
    "f5",
    "f6",
    "f7",
    "f8",
    "f9",
    "f10",
    "f11",
    "f12",
    "`",
    "-",
    "=",
    "[",
    "]",
    "\\",
    ";",
    "'",
    ",",
    ".",
    "/",
    "!",
    "@",
    "#",
    "$",
    "%",
    "^",
    "&",
    "*",
    "(",
    ")",
    "_",
    "+",
    "|",
    "~",
    "{",
    "}",
    ":",
    "<",
    ">",
    "?",
];

/// Hand-rolled equivalent of the `KeyIdSchema` regular expression.
pub fn is_valid_key_id(value: &str) -> bool {
    let parts: Vec<&str> = value.split('+').collect();
    let Some(base) = parts.last() else {
        return false;
    };
    if base.is_empty() {
        return false;
    }
    let valid_base = (base.len() == 1
        && base
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()))
        || BASE_KEYS.contains(base);
    if !valid_base {
        return false;
    }
    let mut seen = HashSet::new();
    for modifier in &parts[..parts.len() - 1] {
        if !matches!(*modifier, "ctrl" | "shift" | "alt" | "super") || !seen.insert(*modifier) {
            return false;
        }
    }
    parts.len() - 1 <= 4
}
pub const PRIVILEGED_ACTIONS: &[&str] = &[
    "app.clipboard.pasteImage",
    "app.editor.external",
    "app.suspend",
    "app.message.copy",
    "p2.attach",
];
pub fn is_privileged(id: &str) -> bool {
    PRIVILEGED_ACTIONS.contains(&id)
}
pub struct SanitizedKeybindings {
    pub config: KeybindingsConfig,
    pub invalid: Vec<String>,
}
pub fn sanitize_keybindings(raw: &Value) -> SanitizedKeybindings {
    let mut result = SanitizedKeybindings {
        config: IndexMap::new(),
        invalid: Vec::new(),
    };
    let Some(obj) = raw.as_object() else {
        return result;
    };
    let migrations = legacy_map();
    for (name, value) in obj {
        let id = migrations.get(name.as_str()).copied().unwrap_or(name);
        if name == "$schema" || (id != name && obj.contains_key(id)) {
            continue;
        }
        let valid_id = id.len() <= 128
            && id.contains('.')
            && id.split('.').all(|s| {
                s.starts_with(|c: char| c.is_ascii_alphabetic())
                    && s.chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            });
        let keys = serde_json::from_value::<KeyIdValue>(value.clone()).ok();
        if let Some(keys) = keys.filter(|v| {
            valid_id
                && v.as_slice().len() <= 8
                && v.as_slice()
                    .iter()
                    .all(|s| s.len() <= 64 && is_valid_key_id(s))
        }) {
            result.config.insert(id.into(), keys);
        } else {
            result.invalid.push(name.chars().take(128).collect());
        }
    }
    result
}
pub struct PrivilegedResolution {
    pub keys: IndexMap<String, Vec<String>>,
    pub lost: Vec<(String, String, String)>,
}
pub fn resolve_privileged_keys(
    definitions: &IndexMap<&str, KeybindingDefinition>,
    user: &KeybindingsConfig,
) -> PrivilegedResolution {
    let effective = |id: &str| {
        user.get(id)
            .or_else(|| definitions.get(id).map(|d| &d.default_keys))
            .map(KeyIdValue::as_slice)
            .unwrap_or_default()
    };
    let mut result = PrivilegedResolution {
        keys: IndexMap::new(),
        lost: Vec::new(),
    };
    for &id in PRIVILEGED_ACTIONS {
        let mut kept = Vec::new();
        for key in effective(id) {
            let mut others = definitions
                .keys()
                .copied()
                .chain(user.keys().map(String::as_str));
            let collision = others.find(|other| {
                !is_privileged(other)
                    && effective(other).contains(key)
                    && (user.contains_key(id) || user.contains_key(*other))
            });
            if let Some(other) = collision {
                result.lost.push((id.into(), key.clone(), other.into()));
            } else {
                kept.push(key.clone());
            }
        }
        result.keys.insert(id.into(), kept);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_exit_binding_wins_over_legacy_alias_and_invalid_chords_are_reported() {
        let result = sanitize_keybindings(
            &serde_json::json!({"exit":"ctrl+c","app.exit":"ctrl+d","plugin.bad":["shift+shift+x"]}),
        );
        assert_eq!(
            result.config.get("app.exit"),
            Some(&KeyIdValue::One("ctrl+d".into()))
        );
        assert_eq!(result.invalid, ["plugin.bad"]);
    }
    #[test]
    fn plugin_override_displaces_launcher_attachment_key() {
        let defaults = builtin_keybindings(KeyPlatform::default());
        let user =
            sanitize_keybindings(&serde_json::json!({"p2.attach":"f3","plugin.pick":"f3"})).config;
        let resolution = resolve_privileged_keys(&defaults, &user);
        assert!(resolution.keys["p2.attach"].is_empty());
        assert!(
            resolution
                .lost
                .contains(&("p2.attach".into(), "f3".into(), "plugin.pick".into()))
        );
    }
}
