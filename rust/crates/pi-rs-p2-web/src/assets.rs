//! Exact copies of upstream `apps/p2/src/web/` CSS, embedded with `include_str!` so `p2`'s
//! server can serve byte-for-byte drop-in styling (ADR-003). The files under `assets/`
//! are unmodified copies of the upstream stylesheets.

/// `@layer` declaration and Elanor/design-token imports (`elanor.css`).
pub const ELANOR_CSS: &str = include_str!("../assets/elanor.css");
/// p2's additions to Elanor's Markdown components (`elanor-markdown.css`).
pub const ELANOR_MARKDOWN_CSS: &str = include_str!("../assets/elanor-markdown.css");
/// p2's element resets and unlayered component styles (`style.css`).
pub const STYLE_CSS: &str = include_str!("../assets/style.css");
/// Owner notices (`owner-notices.css`).
pub const OWNER_NOTICES_CSS: &str = include_str!("../assets/owner-notices.css");
/// Activity wheels, tickers and sheet styles (`activity/activity.css`).
pub const ACTIVITY_CSS: &str = include_str!("../assets/activity/activity.css");
/// Command menu dialog and list styles (`command-menu/command-menu.css`).
pub const COMMAND_MENU_CSS: &str = include_str!("../assets/command-menu/command-menu.css");
/// Elanor's Drawer/Ticker extras (`elanor-extra/elanor-extra.css`).
pub const ELANOR_EXTRA_CSS: &str = include_str!("../assets/elanor-extra/elanor-extra.css");

/// Every embedded stylesheet, with the path `p2` serves it under. Kept in a stable order
/// so the server can emit the same `<link>` sequence as upstream (`elanor.css` first).
pub const STYLESHEETS: &[(&str, &str)] = &[
    ("elanor.css", ELANOR_CSS),
    ("elanor-markdown.css", ELANOR_MARKDOWN_CSS),
    ("style.css", STYLE_CSS),
    ("owner-notices.css", OWNER_NOTICES_CSS),
    ("activity/activity.css", ACTIVITY_CSS),
    ("command-menu/command-menu.css", COMMAND_MENU_CSS),
    ("elanor-extra/elanor-extra.css", ELANOR_EXTRA_CSS),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_stylesheets_are_non_empty_and_distinct() {
        assert_eq!(STYLESHEETS.len(), 7);
        let mut seen = std::collections::HashSet::new();
        for (name, css) in STYLESHEETS {
            assert!(!css.is_empty(), "{name} is empty");
            assert!(css.len() > 20, "{name} is too short to be a stylesheet");
            assert!(seen.insert(*css), "{name} duplicates another stylesheet");
        }
    }

    #[test]
    fn elanor_declares_layer_and_token_imports() {
        assert!(ELANOR_CSS.contains("@layer"));
        assert!(ELANOR_CSS.contains("design-tokens/pi.css"));
    }

    #[test]
    fn markdown_css_defines_markdown_token_rules() {
        assert!(ELANOR_MARKDOWN_CSS.contains(".elanor-markdown"));
        assert!(ELANOR_MARKDOWN_CSS.contains("li.task"));
    }

    #[test]
    fn component_stylesheets_define_their_signature_classes() {
        assert!(ACTIVITY_CSS.contains("activity"));
        assert!(COMMAND_MENU_CSS.contains("command-menu"));
        assert!(ELANOR_EXTRA_CSS.contains("drawer"));
        assert!(STYLE_CSS.contains("--bg"));
    }
}
