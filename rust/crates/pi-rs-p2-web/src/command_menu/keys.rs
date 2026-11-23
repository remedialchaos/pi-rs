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
];

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
    }
}
