//! Rust port of `packages/coding-agent/src/core/keybindings.ts` and the inherited
//! `packages/tui/src/keybindings.ts` behaviour.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::LazyLock;

use indexmap::IndexMap;
use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// A keybinding value: either one key id or a list of alternatives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyIdValue {
    One(String),
    Many(Vec<String>),
}

impl KeyIdValue {
    pub fn keys(&self) -> Vec<String> {
        match self {
            Self::One(key) => vec![key.clone()],
            Self::Many(keys) => keys.clone(),
        }
    }
}

impl From<&str> for KeyIdValue {
    fn from(value: &str) -> Self {
        Self::One(value.to_string())
    }
}

impl From<Vec<String>> for KeyIdValue {
    fn from(value: Vec<String>) -> Self {
        Self::Many(value)
    }
}

impl Serialize for KeyIdValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::One(key) => serializer.serialize_str(key),
            Self::Many(keys) => serializer.collect_seq(keys),
        }
    }
}

impl<'de> Deserialize<'de> for KeyIdValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct KeyIdValueVisitor;

        impl<'de> Visitor<'de> for KeyIdValueVisitor {
            type Value = KeyIdValue;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a key id string or an array of key id strings")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(KeyIdValue::One(value.to_string()))
            }

            fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(KeyIdValue::One(value))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut keys = Vec::new();
                while let Some(key) = seq.next_element::<String>()? {
                    keys.push(key);
                }
                Ok(KeyIdValue::Many(keys))
            }
        }

        deserializer.deserialize_any(KeyIdValueVisitor)
    }
}

/// A keybinding definition: platform-specific default keys plus a description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingDefinition {
    pub default_keys: KeyIdValue,
    pub description: &'static str,
}

/// Platform facts needed to choose default keybindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformDefaults {
    pub platform: &'static str,
    pub windows_keybindings: bool,
    pub is_win32: bool,
    pub is_darwin: bool,
    pub is_wsl: bool,
}

impl PlatformDefaults {
    /// Port of `useWindowsKeybindings` (WT_SESSION is deliberately ignored).
    pub fn detect(platform: &str, env: &HashMap<String, String>) -> Self {
        let is_win32 = platform == "win32";
        let is_wsl = platform == "linux"
            && (env.get("WSL_DISTRO_NAME").is_some_and(|v| !v.is_empty())
                || env.get("WSL_INTEROP").is_some_and(|v| !v.is_empty()));
        Self {
            platform: if is_win32 {
                "win32"
            } else if platform == "darwin" {
                "darwin"
            } else if platform == "linux" {
                "linux"
            } else {
                "other"
            },
            windows_keybindings: is_win32 || is_wsl,
            is_win32,
            is_darwin: platform == "darwin",
            is_wsl,
        }
    }

    /// Reads the current process platform and environment once, at startup.
    pub fn from_env() -> Self {
        let platform: &'static str = if cfg!(target_os = "windows") {
            "win32"
        } else if cfg!(target_os = "macos") {
            "darwin"
        } else if cfg!(target_os = "linux") {
            "linux"
        } else {
            "other"
        };
        let env: HashMap<String, String> = std::env::vars().collect();
        Self::detect(platform, &env)
    }
}

/// Port of `useWindowsKeybindings(platform, env)`.
pub fn use_windows_keybindings(platform: &str, env: &HashMap<String, String>) -> bool {
    PlatformDefaults::detect(platform, env).windows_keybindings
}

/// Convenience for tests that need an environment map from literal pairs.
pub fn env_map(entries: &[(&str, &str)]) -> HashMap<String, String> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect()
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
pub fn build_keybindings(
    defaults: &PlatformDefaults,
) -> IndexMap<&'static str, KeybindingDefinition> {
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
    let undo_defaults = if defaults.is_win32 {
        one("ctrl+z")
    } else if defaults.windows_keybindings {
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
    let previous_prompt = if defaults.windows_keybindings {
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
    let next_prompt = if defaults.windows_keybindings {
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
    let search = if defaults.windows_keybindings {
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
    let suspend = if defaults.is_win32 {
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
    let cycle_backward = if defaults.windows_keybindings {
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
    let follow_up = if defaults.windows_keybindings {
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
    let dequeue = if defaults.windows_keybindings {
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
    let paste_image = if defaults.windows_keybindings {
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
    let fold = if defaults.is_darwin {
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
    let unfold = if defaults.is_darwin {
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

    map
}

/// The full registry, computed once with the startup platform and environment.
pub static KEYBINDINGS: LazyLock<IndexMap<&'static str, KeybindingDefinition>> =
    LazyLock::new(|| build_keybindings(&PlatformDefaults::from_env()));

macro_rules! app_keybindings {
    ($(($variant:ident, $id:literal)),* $(,)?) => {
        /// The 43 application-level binding ids.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum AppKeybinding {
            $($variant),*
        }

        impl AppKeybinding {
            pub const ALL: &'static [Self] = &[$(Self::$variant),*];

            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $id),* }
            }
        }

        impl FromStr for AppKeybinding {
            type Err = String;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $($id => Ok(Self::$variant),)*
                    other => Err(format!("unknown app keybinding: {other}")),
                }
            }
        }

        impl fmt::Display for AppKeybinding {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

app_keybindings!(
    (Interrupt, "app.interrupt"),
    (Clear, "app.clear"),
    (Exit, "app.exit"),
    (Suspend, "app.suspend"),
    (ThinkingCycle, "app.thinking.cycle"),
    (ThinkingSave, "app.thinking.save"),
    (ModelCycleForward, "app.model.cycleForward"),
    (ModelCycleBackward, "app.model.cycleBackward"),
    (ModelSelect, "app.model.select"),
    (ToolsExpand, "app.tools.expand"),
    (ThinkingToggle, "app.thinking.toggle"),
    (SessionToggleNamedFilter, "app.session.toggleNamedFilter"),
    (EditorExternal, "app.editor.external"),
    (MessageCopy, "app.message.copy"),
    (MessageFollowUp, "app.message.followUp"),
    (MessageDequeue, "app.message.dequeue"),
    (ClipboardPasteImage, "app.clipboard.pasteImage"),
    (SessionNew, "app.session.new"),
    (SessionTree, "app.session.tree"),
    (SessionFork, "app.session.fork"),
    (SessionResume, "app.session.resume"),
    (TreeFoldOrUp, "app.tree.foldOrUp"),
    (TreeUnfoldOrDown, "app.tree.unfoldOrDown"),
    (TreeEditLabel, "app.tree.editLabel"),
    (TreeToggleLabelTimestamp, "app.tree.toggleLabelTimestamp"),
    (SessionTogglePath, "app.session.togglePath"),
    (SessionToggleSort, "app.session.toggleSort"),
    (SessionRename, "app.session.rename"),
    (SessionDelete, "app.session.delete"),
    (SessionDeleteNoninvasive, "app.session.deleteNoninvasive"),
    (ModelsSave, "app.models.save"),
    (ModelsEnableAll, "app.models.enableAll"),
    (ModelsClearAll, "app.models.clearAll"),
    (ModelsToggleProvider, "app.models.toggleProvider"),
    (ModelsReorderUp, "app.models.reorderUp"),
    (ModelsReorderDown, "app.models.reorderDown"),
    (TreeFilterDefault, "app.tree.filter.default"),
    (TreeFilterNoTools, "app.tree.filter.noTools"),
    (TreeFilterUserOnly, "app.tree.filter.userOnly"),
    (TreeFilterLabeledOnly, "app.tree.filter.labeledOnly"),
    (TreeFilterAll, "app.tree.filter.all"),
    (TreeFilterCycleForward, "app.tree.filter.cycleForward"),
    (TreeFilterCycleBackward, "app.tree.filter.cycleBackward"),
);

/// Legacy binding name -> current binding id (59 entries).
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

static LEGACY_MAP: LazyLock<IndexMap<&'static str, &'static str>> = LazyLock::new(legacy_map);

/// Migrate legacy binding names and order the result by the registry declaration order.
pub fn migrate_keybindings_config(
    raw: &IndexMap<String, Value>,
) -> (IndexMap<String, Value>, bool) {
    let mut migrated = false;
    let mut config = IndexMap::new();

    for (key, value) in raw {
        let next_key = LEGACY_MAP
            .get(key.as_str())
            .copied()
            .unwrap_or(key.as_str());
        if next_key != key.as_str() {
            migrated = true;
        }
        if next_key != key.as_str() && raw.contains_key(next_key) {
            migrated = true;
            continue;
        }
        config.insert(next_key.to_string(), value.clone());
    }

    let mut ordered = IndexMap::new();
    for id in KEYBINDINGS.keys() {
        if let Some(value) = config.shift_remove(*id) {
            ordered.insert((*id).to_string(), value);
        }
    }
    let mut extras: Vec<String> = config.keys().cloned().collect();
    extras.sort();
    for key in extras {
        if let Some(value) = config.shift_remove(&key) {
            ordered.insert(key, value);
        }
    }

    (ordered, migrated)
}

/// Names accepted as a base key in a key id (the TUI `Key` enum string values).
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

/// Filter a raw JSON object to valid binding values, preserving file order.
pub fn to_keybindings_config(raw: &IndexMap<String, Value>) -> IndexMap<String, KeyIdValue> {
    let mut config = IndexMap::new();
    for (key, value) in raw {
        if key == "$schema" {
            continue;
        }
        let binding = match value {
            Value::String(key_id) if is_valid_key_id(key_id) => {
                Some(KeyIdValue::One(key_id.clone()))
            }
            Value::Array(values) => {
                let keys: Option<Vec<String>> = values
                    .iter()
                    .map(|item| {
                        item.as_str()
                            .filter(|key| is_valid_key_id(key))
                            .map(str::to_string)
                    })
                    .collect();
                keys.map(KeyIdValue::Many)
            }
            _ => None,
        };
        if let Some(binding) = binding {
            config.insert(key.clone(), binding);
        }
    }
    config
}

fn strip_bom(content: &str) -> &str {
    content.strip_prefix('\u{feff}').unwrap_or(content)
}

/// `loadRawConfig`: a missing, malformed, or non-object file yields no bindings.
pub fn load_raw_config(path: &Path) -> Option<IndexMap<String, Value>> {
    if !path.exists() {
        return None;
    }
    let content = fs::read_to_string(path).ok()?;
    let parsed: Value = serde_json::from_str(strip_bom(&content)).ok()?;
    match parsed {
        Value::Object(map) => Some(IndexMap::from_iter(map)),
        Value::Array(_) => Some(IndexMap::new()),
        _ => None,
    }
}

fn load_from_file(path: &Path) -> IndexMap<String, KeyIdValue> {
    let Some(raw) = load_raw_config(path) else {
        return IndexMap::new();
    };
    to_keybindings_config(&migrate_keybindings_config(&raw).0)
}

/// Two keybindings claiming the same key. Conflicts are reported, never evicted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingConflict {
    pub key: String,
    pub keybindings: Vec<String>,
}

/// Manages definitions, user overrides, resolved keys, and reporting conflicts.
#[derive(Debug)]
pub struct KeybindingsManager {
    definitions: IndexMap<String, KeybindingDefinition>,
    user_bindings: IndexMap<String, KeyIdValue>,
    config_path: Option<PathBuf>,
    keys_by_id: HashMap<String, Vec<String>>,
    conflicts: Vec<KeybindingConflict>,
}

impl KeybindingsManager {
    pub fn new(user_bindings: IndexMap<String, KeyIdValue>, config_path: Option<PathBuf>) -> Self {
        let definitions = KEYBINDINGS
            .iter()
            .map(|(id, definition)| ((*id).to_string(), definition.clone()))
            .collect();
        let mut manager = Self {
            definitions,
            user_bindings,
            config_path,
            keys_by_id: HashMap::new(),
            conflicts: Vec::new(),
        };
        manager.rebuild();
        manager
    }

    /// Build a manager for a specific platform/environment, useful for tests.
    pub fn with_defaults(
        defaults: &PlatformDefaults,
        user_bindings: IndexMap<String, KeyIdValue>,
        config_path: Option<PathBuf>,
    ) -> Self {
        let definitions = build_keybindings(defaults)
            .into_iter()
            .map(|(id, definition)| (id.to_string(), definition))
            .collect();
        let mut manager = Self {
            definitions,
            user_bindings,
            config_path,
            keys_by_id: HashMap::new(),
            conflicts: Vec::new(),
        };
        manager.rebuild();
        manager
    }

    /// `KeybindingsManager.create(agentDir)`: config at `agentDir/keybindings.json`.
    pub fn create(agent_dir: &Path) -> Self {
        let config_path = agent_dir.join("keybindings.json");
        let user_bindings = load_from_file(&config_path);
        Self::new(user_bindings, Some(config_path))
    }

    pub fn reload(&mut self) {
        let Some(config_path) = self.config_path.as_deref() else {
            return;
        };
        self.set_user_bindings(load_from_file(config_path));
    }

    fn rebuild(&mut self) {
        self.keys_by_id.clear();
        self.conflicts.clear();

        let mut user_claims: HashMap<String, HashSet<String>> = HashMap::new();
        let mut claim_order: Vec<String> = Vec::new();
        for (id, value) in &self.user_bindings {
            if !self.definitions.contains_key(id) {
                continue;
            }
            for key in normalize_keys(value) {
                let is_new_key = !user_claims.contains_key(&key);
                let claimants = user_claims.entry(key.clone()).or_default();
                claimants.insert(id.clone());
                if is_new_key {
                    claim_order.push(key);
                }
            }
        }
        for key in claim_order {
            if let Some(claimants) = user_claims.get(&key)
                && claimants.len() > 1
            {
                let mut names: Vec<String> = claimants.iter().cloned().collect();
                names.sort();
                self.conflicts.push(KeybindingConflict {
                    key,
                    keybindings: names,
                });
            }
        }

        for (id, definition) in &self.definitions {
            let keys = match self.user_bindings.get(id) {
                Some(value) => normalize_keys(value),
                None => normalize_keys(&definition.default_keys),
            };
            self.keys_by_id.insert(id.clone(), keys);
        }
    }

    pub fn matches(&self, data: &str, id: &str) -> bool {
        self.keys_by_id
            .get(id)
            .is_some_and(|keys| keys.iter().any(|key| matches_key(data, key)))
    }

    pub fn get_keys(&self, id: &str) -> Vec<String> {
        self.keys_by_id.get(id).cloned().unwrap_or_default()
    }

    pub fn get_definition(&self, id: &str) -> Option<&KeybindingDefinition> {
        self.definitions.get(id)
    }

    pub fn get_conflicts(&self) -> &[KeybindingConflict] {
        &self.conflicts
    }

    pub fn set_user_bindings(&mut self, user_bindings: IndexMap<String, KeyIdValue>) {
        self.user_bindings = user_bindings;
        self.rebuild();
    }

    pub fn get_user_bindings(&self) -> &IndexMap<String, KeyIdValue> {
        &self.user_bindings
    }

    /// Effective (resolved) config: one key as a scalar, otherwise an array.
    pub fn get_effective_config(&self) -> IndexMap<String, KeyIdValue> {
        let mut resolved = IndexMap::new();
        for id in self.definitions.keys() {
            let keys = self.keys_by_id.get(id).cloned().unwrap_or_default();
            if keys.len() == 1 {
                resolved.insert(id.clone(), KeyIdValue::One(keys[0].clone()));
            } else {
                resolved.insert(id.clone(), KeyIdValue::Many(keys));
            }
        }
        resolved
    }
}

pub fn normalize_keys(value: &KeyIdValue) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for key in value.keys() {
        if seen.insert(key.clone()) {
            result.push(key);
        }
    }
    result
}

/// Parse a key id into a base key and modifier flags (same rules as upstream `parseKeyId`).
fn parse_key_id(key_id: &str) -> Option<(String, bool, bool, bool, bool)> {
    let mut key = key_id.to_ascii_lowercase();
    let mut ctrl = false;
    let mut shift = false;
    let mut alt = false;
    let mut super_key = false;
    loop {
        let modifier = ["shift", "alt", "ctrl", "super"]
            .into_iter()
            .find(|name| key.starts_with(&format!("{name}+")));
        let Some(modifier) = modifier else {
            break;
        };
        match modifier {
            "ctrl" if !ctrl => ctrl = true,
            "shift" if !shift => shift = true,
            "alt" if !alt => alt = true,
            "super" if !super_key => super_key = true,
            _ => return None,
        }
        key = key[modifier.len() + 1..].to_string();
    }
    if key.is_empty() || (key.contains('+') && key != "+") {
        return None;
    }
    Some((key, ctrl, shift, alt, super_key))
}

fn kitty_modifier_bits(ctrl: bool, shift: bool, alt: bool, super_key: bool) -> u32 {
    1 + u32::from(shift) + 2 * u32::from(alt) + 4 * u32::from(ctrl) + 8 * u32::from(super_key)
}

fn kitty_codepoint(key: &str) -> u32 {
    match key {
        "escape" | "esc" => 27,
        "enter" | "return" => 13,
        "tab" => 9,
        "space" => 32,
        "backspace" => 127,
        "up" => 57352,
        "down" => 57351,
        "right" => 57349,
        "left" => 57348,
        "delete" => 57344,
        "insert" => 57346,
        "pageUp" => 57354,
        "pageDown" => 57353,
        "home" => 57355,
        "end" => 57356,
        other => other.chars().next().map_or(0, u32::from),
    }
}

fn matches_kitty_sequence(data: &str, codepoint: u32, modifier: u32) -> bool {
    let Some(rest) = data.strip_prefix("\x1b[") else {
        return false;
    };
    let Some(sequence) = rest.strip_suffix('u') else {
        return false;
    };
    let mut fields = sequence.split(';');
    let Some(actual_codepoint) = fields.next().and_then(|field| field.parse::<u32>().ok()) else {
        return false;
    };
    let actual_modifier = fields
        .next()
        .and_then(|field| field.parse::<u32>().ok())
        .unwrap_or(1);
    actual_codepoint == codepoint && actual_modifier == modifier
}

fn matches_modify_other_keys(data: &str, codepoint: u32, modifier: u32) -> bool {
    let Some(rest) = data.strip_prefix("\x1b[27;") else {
        return false;
    };
    let Some(sequence) = rest.strip_suffix('~') else {
        return false;
    };
    let mut fields = sequence.split(';');
    let actual_modifier = fields.next().and_then(|field| field.parse::<u32>().ok());
    let actual_codepoint = fields.next().and_then(|field| field.parse::<u32>().ok());
    actual_modifier == Some(modifier) && actual_codepoint == Some(codepoint)
}

/// Match raw terminal input against a key id. Covers legacy sequences, Kitty CSI-u,
/// and xterm modifyOtherKeys, which are the forms upstream handles.
pub fn matches_key(data: &str, key_id: &str) -> bool {
    let Some((key, ctrl, shift, alt, super_key)) = parse_key_id(key_id) else {
        return false;
    };
    let modifiers = kitty_modifier_bits(ctrl, shift, alt, super_key);
    let codepoint = kitty_codepoint(&key);

    if matches_kitty_sequence(data, codepoint, modifiers)
        || matches_modify_other_keys(data, codepoint, modifiers)
    {
        return true;
    }

    match key.as_str() {
        "escape" | "esc" if !ctrl && !shift && !alt && !super_key => data == "\x1b",
        "enter" | "return" => match (ctrl, shift, alt, super_key) {
            (false, false, false, false) => data == "\r" || data == "\n" || data == "\x1bOM",
            (false, true, false, false) => data == "\n" || data == "\x1b\r",
            (false, false, true, false) => data == "\x1b\r",
            _ => false,
        },
        "tab" => match (ctrl, shift, alt, super_key) {
            (false, false, false, false) => data == "\t",
            (false, true, false, false) => data == "\x1b[Z",
            _ => false,
        },
        "space" if !ctrl && !shift && !alt && !super_key => data == " ",
        "backspace" if !ctrl && !shift && alt && !super_key => {
            data == "\x1b\x7f" || data == "\x1b\x08"
        }
        "backspace" if !ctrl && !shift && !alt && !super_key => data == "\x7f" || data == "\x08",
        "up" | "down" | "left" | "right" if !ctrl && !shift && !alt && !super_key => {
            let sequence = match key.as_str() {
                "up" => "\x1b[A",
                "down" => "\x1b[B",
                "right" => "\x1b[C",
                _ => "\x1b[D",
            };
            data == sequence
        }
        _ => {
            let is_printable = key.len() == 1;
            if !is_printable {
                return false;
            }
            if alt && !ctrl && !shift && !super_key && data == format!("\x1b{key}") {
                return true;
            }
            if ctrl && !alt && !shift && !super_key {
                let byte = key.as_bytes()[0];
                if byte.is_ascii_lowercase() && data.as_bytes() == [byte - b'a' + 1] {
                    return true;
                }
                if key == "j" && data == "\n" {
                    return true;
                }
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(pairs: &[(&str, Value)]) -> IndexMap<String, Value> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect()
    }

    #[test]
    fn windows_native_platform() {
        assert!(use_windows_keybindings("win32", &HashMap::new()));
    }

    #[test]
    fn wsl_detected_without_wt_session() {
        assert!(use_windows_keybindings(
            "linux",
            &env_map(&[("WSL_DISTRO_NAME", "Ubuntu")])
        ));
        assert!(use_windows_keybindings(
            "linux",
            &env_map(&[("WSL_INTEROP", "/run/WSL/123_interop")])
        ));
    }

    #[test]
    fn wt_session_alone_is_ignored() {
        assert!(!use_windows_keybindings(
            "linux",
            &env_map(&[("WT_SESSION", "session")])
        ));
    }

    #[test]
    fn other_platforms_do_not_use_windows_keys() {
        assert!(!use_windows_keybindings("linux", &HashMap::new()));
        assert!(!use_windows_keybindings("darwin", &HashMap::new()));
    }

    #[test]
    fn all_43_app_ids_are_registered() {
        let definitions = build_keybindings(&PlatformDefaults::detect("linux", &HashMap::new()));
        assert_eq!(AppKeybinding::ALL.len(), 43);
        for id in AppKeybinding::ALL {
            assert!(definitions.contains_key(id.as_str()), "missing {id}");
        }
        assert_eq!(
            definitions
                .keys()
                .filter(|id| id.starts_with("app."))
                .count(),
            43
        );
    }

    #[test]
    fn platform_defaults_match_upstream_branches() {
        let wsl = PlatformDefaults::detect("linux", &env_map(&[("WSL_DISTRO_NAME", "Ubuntu")]));
        let wsl_bindings = build_keybindings(&wsl);
        assert_eq!(
            wsl_bindings["app.clipboard.pasteImage"].default_keys,
            many(&["alt+v", "ctrl+v"])
        );
        assert_eq!(
            wsl_bindings["tui.altScreen.search"].default_keys,
            one("ctrl+f")
        );
        assert_eq!(
            wsl_bindings["app.message.followUp"].default_keys,
            one("ctrl+q")
        );
        assert_eq!(
            wsl_bindings["app.model.cycleBackward"].default_keys,
            one("alt+p")
        );
        assert_eq!(wsl_bindings["tui.editor.undo"].default_keys, one("alt+z"));

        let win = build_keybindings(&PlatformDefaults::detect("win32", &HashMap::new()));
        assert_eq!(win["app.suspend"].default_keys, empty());
        assert_eq!(win["tui.editor.undo"].default_keys, one("ctrl+z"));

        let mac = build_keybindings(&PlatformDefaults::detect("darwin", &HashMap::new()));
        assert_eq!(
            mac["app.tree.foldOrUp"].default_keys,
            many(&["alt+left", "ctrl+left"])
        );
        let linux = build_keybindings(&PlatformDefaults::detect("linux", &HashMap::new()));
        assert_eq!(
            linux["app.tree.foldOrUp"].default_keys,
            many(&["ctrl+left", "alt+left"])
        );
        assert_eq!(
            linux["tui.altScreen.previousPrompt"].default_keys,
            many(&["ctrl+shift+up", "ctrl+up"])
        );
        assert_eq!(linux["app.message.dequeue"].default_keys, one("alt+up"));
    }

    #[test]
    fn migration_rewrites_old_names() {
        let (config, migrated) = migrate_keybindings_config(&raw(&[
            ("cursorUp", serde_json::json!(["up", "ctrl+p"])),
            ("expandTools", serde_json::json!("ctrl+x")),
        ]));
        assert!(migrated);
        assert_eq!(
            config["tui.editor.cursorUp"],
            serde_json::json!(["up", "ctrl+p"])
        );
        assert_eq!(config["app.tools.expand"], serde_json::json!("ctrl+x"));
    }

    #[test]
    fn migration_prefers_namespaced_key() {
        let (config, migrated) = migrate_keybindings_config(&raw(&[
            ("expandTools", serde_json::json!("ctrl+x")),
            ("app.tools.expand", serde_json::json!("ctrl+y")),
        ]));
        assert!(migrated);
        assert_eq!(config.len(), 1);
        assert_eq!(config["app.tools.expand"], serde_json::json!("ctrl+y"));
    }

    #[test]
    fn migration_orders_unknown_keys_lexicographically() {
        let (config, _) = migrate_keybindings_config(&raw(&[
            ("zzz", serde_json::json!("a")),
            ("app.interrupt", serde_json::json!("b")),
            ("aaa", serde_json::json!("c")),
        ]));
        let keys: Vec<&String> = config.keys().collect();
        assert_eq!(keys[0], "app.interrupt");
        assert_eq!(keys[1], "aaa");
        assert_eq!(keys[2], "zzz");
    }

    #[test]
    fn key_id_validation_rejects_duplicate_modifiers() {
        assert!(is_valid_key_id("ctrl+shift+p"));
        assert!(is_valid_key_id("pageUp"));
        assert!(!is_valid_key_id("ctrl+ctrl+p"));
        assert!(!is_valid_key_id("ctrl+"));
        assert!(!is_valid_key_id("A"));
    }

    #[test]
    fn rebinding_does_not_evict_other_bindings_and_reports_conflicts() {
        let mut bindings = IndexMap::new();
        bindings.insert(
            "tui.input.submit".to_string(),
            KeyIdValue::One("ctrl+x".to_string()),
        );
        bindings.insert(
            "tui.select.confirm".to_string(),
            KeyIdValue::One("ctrl+x".to_string()),
        );
        let manager = KeybindingsManager::new(bindings, None);
        assert_eq!(manager.get_keys("tui.input.submit"), vec!["ctrl+x"]);
        assert_eq!(manager.get_keys("tui.select.confirm"), vec!["ctrl+x"]);
        assert_eq!(manager.get_conflicts().len(), 1);
        assert_eq!(manager.get_conflicts()[0].key, "ctrl+x");
    }

    #[test]
    fn matches_legacy_and_kitty_keys() {
        assert!(matches_key("\n", "shift+enter"));
        assert!(matches_key("\x1b[106;5u", "ctrl+j"));
        assert!(matches_key("\x03", "ctrl+c"));
        assert!(!matches_key("x", "ctrl+x"));
    }

    #[test]
    fn create_loads_legacy_names_in_memory() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        let config = raw(&[
            ("selectConfirm", serde_json::json!("enter")),
            ("interrupt", serde_json::json!("ctrl+x")),
        ]);
        let content = serde_json::to_string(&config).map_err(|error| format!("json: {error}"))?;
        fs::write(dir.path().join("keybindings.json"), content)
            .map_err(|error| format!("write: {error}"))?;
        let manager = KeybindingsManager::create(dir.path());
        assert_eq!(
            manager.get_user_bindings()["tui.select.confirm"],
            KeyIdValue::One("enter".to_string())
        );
        assert_eq!(
            manager.get_user_bindings()["app.interrupt"],
            KeyIdValue::One("ctrl+x".to_string())
        );
        assert_eq!(
            manager.get_effective_config()["app.interrupt"],
            KeyIdValue::One("ctrl+x".to_string())
        );
        Ok(())
    }
}
