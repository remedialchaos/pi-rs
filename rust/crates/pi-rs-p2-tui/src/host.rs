use crate::core_commands::CoreAction;
use crate::theme::{TerminalState, Theme, ThemeFileError, ThemeRef, watch_theme_file};
use pi_rs_tui::{ComponentRef, Terminal, TuiMainScreen};
use std::{cell::RefCell, rc::Rc, sync::mpsc};
use std::{io, time::Duration};

pub trait TuiHost {
    fn run_core_action(&mut self, action: CoreAction);
}
pub trait TuiContribution {
    fn id(&self) -> &str;
    fn run(&self, host: &mut dyn TuiHost);
}
pub struct TuiClient {
    pub screen: TuiMainScreen,
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
        screen.base.content.add_child(root.clone());
        screen.base.set_focus(Some(root));
        Self {
            screen,
            contributions: Default::default(),
            theme_watch: None,
        }
    }
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
    pub fn step(&mut self, timeout: Duration) -> io::Result<()> {
        if let Some(ThemeWatch {
            target, changes, ..
        }) = &self.theme_watch
            && let Some(theme) = changes.try_iter().filter_map(Result::ok).last()
        {
            target.set(theme);
            self.screen.base.invalidate();
            self.screen.base.request_render(true);
        }
        self.screen.step(timeout)
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
        self.screen.base.invalidate();
        self.screen.base.request_render(true);
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
        self.screen.base.add_input_listener(Box::new(move |data| {
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
