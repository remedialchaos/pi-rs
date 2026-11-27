//! The web's default key table (upstream `command-menu/keys.ts`).

/// A key pressed with the platform's primary modifier and no other modifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyChord {
    pub key: &'static str,
}

/// The default bindings by id.
pub const DEFAULT_KEYS: &[(&str, &[KeyChord])] = &[
    ("p2.commands.open", &[KeyChord { key: "k" }]),
    ("p2.scope-prompt.toggle", &[KeyChord { key: "." }]),
    ("p2.sidebar.toggle", &[KeyChord { key: "b" }]),
    ("p2.terminal.toggle", &[KeyChord { key: "j" }]),
    ("p2.workspace.toggle", &[KeyChord { key: "e" }]),
];

/// Global keyboard shortcut action resolved by [`resolve_shortcut`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShortcutAction {
    OpenCommandMenu,
    ToggleScopePrompt,
    ToggleSidebar,
    ToggleTerminal,
    ToggleWorkspace,
    StopGeneration,
    CloseModal,
}

/// The modifier state of a key press.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub meta: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

/// Whether a key press presses one of `binding`'s chords (`apple`: Cmd on macOS, Ctrl elsewhere).
pub fn matches_binding(key: &str, mods: Modifiers, binding: &str, apple: bool) -> bool {
    let primary = if apple { mods.meta } else { mods.ctrl };
    let other = if apple { mods.ctrl } else { mods.meta };
    if !primary || other || mods.alt || mods.shift {
        return false;
    }
    let key = key.to_lowercase();
    DEFAULT_KEYS
        .iter()
        .find(|(id, _)| *id == binding)
        .is_some_and(|(_, chords)| chords.iter().any(|chord| chord.key == key))
}

/// Resolves global keyboard shortcuts including `Cmd/Ctrl+.` and `Escape`.
pub fn resolve_shortcut(
    key: &str,
    mods: Modifiers,
    apple: bool,
    busy: bool,
    modal_open: bool,
) -> Option<ShortcutAction> {
    if key == "Escape" && !mods.meta && !mods.ctrl && !mods.alt {
        if modal_open {
            return Some(ShortcutAction::CloseModal);
        }
        if busy {
            return Some(ShortcutAction::StopGeneration);
        }
        return None;
    }
    if matches_binding(key, mods, "p2.commands.open", apple) {
        return Some(ShortcutAction::OpenCommandMenu);
    }
    if matches_binding(key, mods, "p2.scope-prompt.toggle", apple) {
        return Some(ShortcutAction::ToggleScopePrompt);
    }
    if matches_binding(key, mods, "p2.sidebar.toggle", apple) {
        return Some(ShortcutAction::ToggleSidebar);
    }
    if matches_binding(key, mods, "p2.terminal.toggle", apple) {
        return Some(ShortcutAction::ToggleTerminal);
    }
    if matches_binding(key, mods, "p2.workspace.toggle", apple) {
        return Some(ShortcutAction::ToggleWorkspace);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_k_on_apple_opens_menu() {
        let mods = Modifiers {
            meta: true,
            ..Default::default()
        };
        assert!(matches_binding("K", mods, "p2.commands.open", true));
        assert!(matches_binding("k", mods, "p2.commands.open", true));
    }

    #[test]
    fn ctrl_k_on_non_apple_opens_menu() {
        let mods = Modifiers {
            ctrl: true,
            ..Default::default()
        };
        assert!(matches_binding("k", mods, "p2.commands.open", false));
    }

    #[test]
    fn extra_modifiers_or_wrong_key_do_not_match() {
        let mut mods = Modifiers {
            meta: true,
            ..Default::default()
        };
        assert!(matches_binding("k", mods, "p2.commands.open", true));
        mods.meta = false;
        mods.ctrl = true;
        assert!(!matches_binding("k", mods, "p2.commands.open", true));
        let mods = Modifiers {
            meta: true,
            shift: true,
            ..Default::default()
        };
        assert!(!matches_binding("k", mods, "p2.commands.open", true));
        let mods = Modifiers {
            meta: true,
            ..Default::default()
        };
        assert!(!matches_binding("x", mods, "p2.commands.open", true));
    }

    #[test]
    fn scope_prompt_binding() {
        let mods = Modifiers {
            meta: true,
            ..Default::default()
        };
        assert!(matches_binding(".", mods, "p2.scope-prompt.toggle", true));
        assert_eq!(
            resolve_shortcut(".", mods, true, false, false),
            Some(ShortcutAction::ToggleScopePrompt)
        );
        assert_eq!(
            resolve_shortcut("Escape", Modifiers::default(), true, true, false),
            Some(ShortcutAction::StopGeneration)
        );
        assert_eq!(
            resolve_shortcut("Escape", Modifiers::default(), true, true, true),
            Some(ShortcutAction::CloseModal)
        );
    }
}
