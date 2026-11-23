use crate::core_commands::CoreAction;
use pi_rs_tui::{ComponentRef, Terminal, TuiMainScreen};
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
    contributions: indexmap::IndexMap<String, Box<dyn TuiContribution>>,
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
        }
    }
    pub fn register(&mut self, contribution: Box<dyn TuiContribution>) -> bool {
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
        self.screen.step(timeout)
    }
}
