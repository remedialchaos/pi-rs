//! The open command menu's context values (upstream `command-menu/context.ts`).

use super::layers::CommandTarget;

/// The state the menu's parts read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandMenuState {
    /// The target captured when the menu opened; gone once the reader removed it.
    pub target: Option<CommandTarget>,
    /// The names of the pages open on top of the root, the open one last.
    pub pages: Vec<Option<String>>,
    /// The open page's query, deferred so lists filter while the input shows every keystroke.
    pub query: String,
}

/// The menu's actions (the host wires them to navigation and command execution).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandMenuActions {
    pub close: bool,
    pub back: bool,
    pub remove_target: bool,
    /// Commands run by id, in order.
    pub run: Vec<String>,
    /// Pages opened, in order.
    pub open_page: Vec<String>,
}

/// How long a pending Enter waits for its rows to render (upstream `ENTER_WAIT_MS`).
pub const ENTER_WAIT_MS: u64 = 1000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_defaults() {
        let state = CommandMenuState::default();
        assert!(state.target.is_none());
        assert!(state.pages.is_empty());
        assert_eq!(state.query, "");
    }
}
