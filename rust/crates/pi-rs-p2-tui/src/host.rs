use crate::core_commands::CoreAction;
use crate::theme::{TerminalState, Theme, ThemeFileError, ThemeRef, watch_theme_file};
use pi_rs_tui::{ComponentRef, Terminal, Tui, TuiAltScreen, TuiMainScreen};
use std::{cell::RefCell, rc::Rc, sync::mpsc};
use std::{io, time::Duration};

pub trait TuiHost {
    fn run_core_action(&mut self, action: CoreAction);
}
pub trait TuiContribution {
    fn id(&self) -> &str;
    fn run(&self, host: &mut dyn TuiHost);
}
pub struct TuiClient<S = TuiMainScreen> {
    pub screen: S,
    contributions: indexmap::IndexMap<String, Rc<dyn TuiContribution>>,
    theme_watch: Option<ThemeWatch>,
}
struct ThemeWatch {
    _watcher: notify::RecommendedWatcher,
    target: ThemeRef,
    changes: mpsc::Receiver<Result<Theme, ThemeFileError>>,
}
impl TuiClient {
    pub fn mount(
        terminal: impl Terminal + 'static,
        root: ComponentRef,
        hardware_cursor: bool,
    ) -> Self {
        let mut screen = TuiMainScreen::new(terminal, hardware_cursor, None);
        screen.base_mut().content.add_child(root.clone());
        screen.base_mut().set_focus(Some(root));
        Self {
            screen,
            contributions: Default::default(),
            theme_watch: None,
        }
    }
    pub fn step(&mut self, timeout: Duration) -> io::Result<()> {
        self.apply_theme_changes();
        self.screen.step(timeout)
    }
}
impl TuiClient<TuiAltScreen> {
    pub fn step(&mut self, timeout: Duration) -> io::Result<()> {
        self.apply_theme_changes();
        self.screen.step(timeout)
    }
    pub fn mount_panel(
        terminal: impl Terminal + 'static,
        header: ComponentRef,
        content: ComponentRef,
        field: ComponentRef,
        mouse: bool,
    ) -> Self {
        let mut screen = TuiAltScreen::new(
            terminal,
            false,
            pi_rs_tui::TuiAltScreenOptions {
                mouse,
                wheel_scroll_lines: pi_rs_tui::WheelScrollLines::Auto,
                ..Default::default()
            },
        );
        use pi_rs_tui::{
            ScrollView, ScrollViewOptions, StackEntryOptions, VStack, ViewportTui,
            layout_node::StackBasis,
        };
        let body = ScrollView::new(
            content,
            ScrollViewOptions {
                primary: true,
                follow_end: true,
                ..Default::default()
            },
        );
        let mut panel = VStack::default();
        panel.add_child(
            header,
            StackEntryOptions {
                basis: StackBasis::Cells(1),
                shrink: 0.,
                ..Default::default()
            },
        );
        panel.add_child(
            ComponentRef::new(body),
            StackEntryOptions {
                basis: StackBasis::Cells(0),
                grow: 1.,
                ..Default::default()
            },
        );
        panel.add_child(
            field.clone(),
            StackEntryOptions {
                shrink: 0.,
                ..Default::default()
            },
        );
        screen.set_layout_root(Some(ComponentRef::new(panel)));
        screen.base.set_focus(Some(field));
        Self {
            screen,
            contributions: Default::default(),
            theme_watch: None,
        }
    }
}
impl<S: Tui> TuiClient<S> {
    pub fn register(&mut self, contribution: Box<dyn TuiContribution>) -> bool {
        self.insert(Rc::from(contribution))
    }
    fn insert(&mut self, contribution: Rc<dyn TuiContribution>) -> bool {
        let id = contribution.id();
        if crate::core_commands::CORE_COMMANDS
            .iter()
            .any(|cmd| cmd.id == id)
            || self.contributions.contains_key(id)
        {
            return false;
        }
        self.contributions.insert(id.into(), contribution);
        true
    }
    pub fn dispatch(&self, id: &str, host: &mut dyn TuiHost) -> bool {
        if let Some(command) = crate::core_commands::CORE_COMMANDS
            .iter()
            .find(|cmd| cmd.id == id)
        {
            host.run_core_action(command.action);
        } else if let Some(contribution) = self.contributions.get(id) {
            contribution.run(host);
        } else {
            return false;
        }
        true
    }
    fn apply_theme_changes(&mut self) {
        if let Some(ThemeWatch {
            target, changes, ..
        }) = &self.theme_watch
            && let Some(theme) = changes.try_iter().filter_map(Result::ok).last()
        {
            target.set(theme);
            pi_rs_tui::Component::invalidate(&mut self.screen);
            self.screen.base_mut().request_render(true);
        }
    }
    pub fn watch_theme(
        &mut self,
        path: &std::path::Path,
        target: ThemeRef,
        mode: pi_rs_tui::TerminalColorMode,
        terminal: TerminalState,
    ) -> Result<(), ThemeFileError> {
        let (send, receive) = mpsc::channel();
        let (initial, watcher) = watch_theme_file(path, mode, terminal, move |result| {
            let _ = send.send(result);
        })?;
        target.set(initial);
        self.theme_watch = Some(ThemeWatch {
            _watcher: watcher,
            target,
            changes: receive,
        });
        pi_rs_tui::Component::invalidate(&mut self.screen);
        self.screen.base_mut().request_render(true);
        Ok(())
    }
    /// Register a plugin command and route its shortcut before focused input.
    pub fn register_shortcut(
        &mut self,
        key: String,
        contribution: Box<dyn TuiContribution>,
        host: Rc<RefCell<dyn TuiHost>>,
    ) -> bool {
        let contribution: Rc<dyn TuiContribution> = Rc::from(contribution);
        if !self.insert(contribution.clone()) {
            return false;
        }
        self.screen
            .base_mut()
            .add_input_listener(Box::new(move |data| {
                let consume = pi_rs_tui::keys::matches_key_data(data, &key);
                if consume {
                    contribution.run(&mut *host.borrow_mut());
                }
                pi_rs_tui::TuiInputListenerResult {
                    consume,
                    data: None,
                }
            }));
        true
    }
}
