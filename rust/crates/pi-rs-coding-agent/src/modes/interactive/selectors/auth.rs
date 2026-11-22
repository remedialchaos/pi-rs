use std::cell::Cell;
use std::rc::Rc;

use pi_rs_agent::CancellationToken;
use pi_rs_ai::auth::{AuthEvent, AuthPrompt, PromptKind};
use pi_rs_tui::{
    Component, ComponentRef, Focusable, get_keybindings, hyperlink, replace_control_characters,
};

use super::{InputDialog, Picker, PickerItem, append_text};
use crate::modes::interactive::theme::Theme;
use crate::utils::web_url::{display_url, web_url};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginDialogAction {
    Cancel,
    OpenUrl(String),
    Copy(String),
}

struct DeviceAction {
    url: Option<String>,
    code: Option<String>,
}

struct PromptPane {
    component: ComponentRef,
    finished: Rc<Cell<bool>>,
    signal: CancellationToken,
}

/// Provider-owned prompts and status events stay independent of the OAuth task.
/// The caller handles browser/clipboard work without blocking terminal input.
pub struct LoginDialog {
    title: String,
    theme: Theme,
    signal: CancellationToken,
    action: Box<dyn FnMut(LoginDialogAction)>,
    rows: Vec<(&'static str, String)>,
    pending: Option<PromptPane>,
    device: Option<DeviceAction>,
    auth_url: Option<String>,
    focused: bool,
    finished: bool,
}

impl LoginDialog {
    pub fn new(
        title: String,
        theme: Theme,
        signal: CancellationToken,
        action: impl FnMut(LoginDialogAction) + 'static,
    ) -> Self {
        Self {
            title,
            theme,
            signal,
            action: Box::new(action),
            rows: Vec::new(),
            pending: None,
            device: None,
            auth_url: None,
            focused: false,
            finished: false,
        }
    }

    pub fn notify(&mut self, event: AuthEvent) {
        if self.finished || self.signal.is_cancelled() {
            return;
        }
        match event {
            AuthEvent::AuthUrl { url, instructions } => {
                self.rows.clear();
                self.device = None;
                self.auth_url = web_url(&url);
                let shown = display_url(&url);
                self.rows.push((
                    "accent",
                    self.auth_url
                        .as_ref()
                        .map_or(shown.clone(), |target| hyperlink(&shown, target)),
                ));
                if let Some(instructions) = instructions {
                    self.rows
                        .push(("warning", replace_control_characters(&instructions)));
                }
                if let Some(target) = &self.auth_url {
                    (self.action)(LoginDialogAction::OpenUrl(target.clone()));
                }
            }
            AuthEvent::DeviceCode {
                user_code,
                verification_uri,
                open_browser_on_confirm,
                copy_code_on_confirm,
                ..
            } => {
                self.rows.clear();
                self.auth_url = None;
                let url = web_url(&verification_uri);
                let shown = display_url(&verification_uri);
                self.rows.push((
                    "accent",
                    url.as_ref()
                        .map_or(shown.clone(), |target| hyperlink(&shown, target)),
                ));
                self.rows.push((
                    "warning",
                    format!("Enter code: {}", replace_control_characters(&user_code)),
                ));
                let action = DeviceAction {
                    url: open_browser_on_confirm.then_some(url).flatten(),
                    code: copy_code_on_confirm.then_some(user_code),
                };
                self.device = (action.url.is_some() || action.code.is_some()).then_some(action);
            }
            AuthEvent::Info { message, links } => {
                self.rows
                    .push(("text", replace_control_characters(&message)));
                for (url, label) in links {
                    let shown = display_url(&url);
                    let text = label.map_or_else(
                        || shown.clone(),
                        |label| format!("{}: {shown}", replace_control_characters(&label)),
                    );
                    let line = match web_url(&url) {
                        Some(target) => hyperlink(&text, &target),
                        None => text,
                    };
                    self.rows.push(("accent", line));
                }
            }
            AuthEvent::Progress(message) => self
                .rows
                .push(("muted", replace_control_characters(&message))),
        }
    }

    pub fn show_prompt(
        &mut self,
        prompt: AuthPrompt,
        mut submit: impl FnMut(Option<String>) + 'static,
    ) {
        if self.finished || self.signal.is_cancelled() || prompt.signal.is_cancelled() {
            submit(None);
            return;
        }
        if let Some(previous) = &mut self.pending {
            previous.component.borrow_mut().handle_input("\x1b");
        }
        self.device = None;
        let finished = Rc::new(Cell::new(false));
        let done = Rc::clone(&finished);
        let mut submit = move |result| {
            done.set(true);
            submit(result);
        };
        let component = match prompt.kind {
            PromptKind::Select(options) => {
                let items = options
                    .into_iter()
                    .map(|option| {
                        let label = option
                            .description
                            .map_or(option.label.clone(), |description| {
                                format!("{} · {description}", option.label)
                            });
                        PickerItem::new(replace_control_characters(&label), option.id)
                    })
                    .collect();
                ComponentRef::new(
                    Picker::with_items(prompt.message, items, self.theme.clone(), move |item| {
                        submit(item.cloned())
                    })
                    .without_filter()
                    .without_wrap(),
                )
            }
            kind => {
                let mut input = InputDialog::new(
                    prompt.message,
                    prompt.placeholder,
                    self.theme.clone(),
                    submit,
                );
                if kind == PromptKind::Secret {
                    input = input.masked();
                }
                ComponentRef::new(input)
            }
        };
        if let Some(focus) = component.borrow_mut().as_focusable() {
            focus.set_focused(self.focused);
        }
        self.pending = Some(PromptPane {
            component,
            finished,
            signal: prompt.signal,
        });
    }

    fn cancel(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.signal.cancel();
        self.device = None;
        if let Some(pending) = self.pending.take() {
            pending.component.borrow_mut().handle_input("\x1b");
        }
        (self.action)(LoginDialogAction::Cancel);
    }
}

impl Component for LoginDialog {
    fn render(&self, width: usize) -> Vec<String> {
        if width == 0 {
            return Vec::new();
        }
        let mut rows = vec![self.theme.fg("border", &"─".repeat(width))];
        append_text(&mut rows, width, &self.theme, "accent", &self.title);
        for (color, text) in &self.rows {
            append_text(&mut rows, width, &self.theme, color, text);
        }
        if let Some(prompt) = &self.pending {
            rows.extend(prompt.component.borrow().render(width));
        } else if self.device.is_some() {
            append_text(
                &mut rows,
                width,
                &self.theme,
                "muted",
                "Enter continue · Esc cancel",
            );
        } else {
            append_text(&mut rows, width, &self.theme, "muted", "Esc cancel");
        }
        rows.push(self.theme.fg("border", &"─".repeat(width)));
        rows
    }

    fn handle_input(&mut self, data: &str) {
        if self.finished {
            return;
        }
        let keys = get_keybindings();
        if keys.matches(data, "tui.select.cancel") {
            self.cancel();
        } else if keys.matches(data, "app.message.copy")
            && let Some(url) = &self.auth_url
        {
            (self.action)(LoginDialogAction::Copy(url.clone()));
        } else if let Some(prompt) = &self.pending {
            prompt.component.borrow_mut().handle_input(data);
            if prompt.finished.get() {
                self.pending = None;
            }
        } else if (keys.matches(data, "tui.select.confirm") || data == "\n")
            && self.focused
            && let Some(device) = self.device.take()
        {
            if let Some(code) = device.code {
                (self.action)(LoginDialogAction::Copy(code));
            }
            if let Some(url) = device.url {
                (self.action)(LoginDialogAction::OpenUrl(url));
            }
        }
    }

    fn tick(&mut self, _now: std::time::Instant) -> bool {
        let prompt_cancelled = self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.signal.is_cancelled());
        if !self.finished && (self.signal.is_cancelled() || prompt_cancelled) {
            self.cancel();
            true
        } else {
            false
        }
    }

    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        Some(self)
    }
    fn captures_viewport_input(&self) -> bool {
        true
    }
}

impl Focusable for LoginDialog {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        if let Some(prompt) = &self.pending
            && let Some(focus) = prompt.component.borrow_mut().as_focusable()
        {
            focus.set_focused(focused);
        }
    }
}
