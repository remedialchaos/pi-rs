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

/// Modifier-only binding specification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModifierTapSpec {
    pub id: &'static str,
    pub primary: bool,
    pub shift: bool,
}

/// Modifier-only bindings by id: pressed together and released with no other key or click in between.
pub const MODIFIER_TAPS: &[ModifierTapSpec] = &[ModifierTapSpec {
    id: "p2.sessions.next",
    primary: true,
    shift: true,
}];

/// Tracks modifier-only taps (e.g. `Cmd+Shift` on macOS, `Ctrl+Shift` elsewhere) while cancelling
/// if any non-modifier key, key repeat, pointer press, or window blur occurs before release.
#[derive(Clone, Debug)]
pub struct ModifierTapTracker {
    spec: ModifierTapSpec,
    apple: bool,
    armed: bool,
}

impl ModifierTapTracker {
    pub fn new(tap_id: &str, apple: bool) -> Option<Self> {
        let spec = MODIFIER_TAPS.iter().find(|s| s.id == tap_id).copied()?;
        Some(Self {
            spec,
            apple,
            armed: false,
        })
    }

    fn is_tap_modifier(&self, key: &str) -> bool {
        let primary_key = if self.apple { "Meta" } else { "Control" };
        key == primary_key || key == "Shift"
    }

    pub fn on_keydown(&mut self, key: &str, mods: Modifiers, repeat: bool) {
        let primary = if self.apple { mods.meta } else { mods.ctrl };
        let other = if self.apple { mods.ctrl } else { mods.meta };
        self.armed = self.is_tap_modifier(key)
            && !repeat
            && primary == self.spec.primary
            && mods.shift == self.spec.shift
            && !other
            && !mods.alt;
    }

    pub fn on_keyup(&mut self, key: &str) -> bool {
        if !self.armed || !self.is_tap_modifier(key) {
            return false;
        }
        self.armed = false;
        true
    }

    pub fn disarm(&mut self) {
        self.armed = false;
    }

    pub fn is_armed(&self) -> bool {
        self.armed
    }
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

    #[test]
    fn modifier_tap_cycles_sessions_on_clean_release_and_disarms_on_interrupts() {
        let mut apple_tracker =
            ModifierTapTracker::new("p2.sessions.next", true).expect("registered tap");
        let cmd_shift = Modifiers {
            meta: true,
            shift: true,
            ..Default::default()
        };
        apple_tracker.on_keydown("Shift", cmd_shift, false);
        assert!(apple_tracker.is_armed());
        assert!(apple_tracker.on_keyup("Shift"));
        assert!(!apple_tracker.on_keyup("Meta"));

        // Non-Apple uses Ctrl+Shift.
        let mut linux_tracker =
            ModifierTapTracker::new("p2.sessions.next", false).expect("registered tap");
        let ctrl_shift = Modifiers {
            ctrl: true,
            shift: true,
            ..Default::default()
        };
        linux_tracker.on_keydown("Control", ctrl_shift, false);
        assert!(linux_tracker.on_keyup("Control"));

        // Cmd+Shift+K disarms.
        apple_tracker.on_keydown("Shift", cmd_shift, false);
        apple_tracker.on_keydown("k", cmd_shift, false);
        assert!(!apple_tracker.on_keyup("Shift"));

        // Key repeat disarms.
        apple_tracker.on_keydown("Shift", cmd_shift, true);
        assert!(!apple_tracker.on_keyup("Shift"));

        // Pointer click / blur disarms.
        apple_tracker.on_keydown("Meta", cmd_shift, false);
        apple_tracker.disarm();
        assert!(!apple_tracker.on_keyup("Meta"));
    }
}
