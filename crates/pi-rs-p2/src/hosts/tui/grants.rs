//! The local key listener grants one clipboard write for the currently displayed answer.
//! Executing synchronously keeps a grant from surviving navigation or terminal shutdown.
use base64::{Engine, engine::general_purpose::STANDARD};
use pi_rs_p2_tui::{KeyPlatform, builtin_keybindings, resolve_privileged_keys};
use pi_rs_tui::{
    Component, Editor,
    keys::matches_key_data,
    native_platform::{ClipboardRead, get_native_clipboard},
};
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Default)]
pub(super) struct CopyGrant {
    answer: String,
    pending: Option<String>,
}
impl CopyGrant {
    pub(super) fn display(&mut self, answer: &str) {
        self.answer = answer.to_owned();
        self.pending = None;
    }
    pub(super) fn take(&mut self) -> Option<String> {
        self.pending.take()
    }
    pub(super) fn listener(
        state: Rc<RefCell<Self>>,
        user: &pi_rs_p2_tui::KeybindingsConfig,
        editor: Rc<RefCell<Editor>>,
    ) -> impl FnMut(&str) -> bool + use<> {
        let bindings = builtin_keybindings(KeyPlatform::default());
        let mut resolved = resolve_privileged_keys(&bindings, user);
        let keys = resolved
            .keys
            .shift_remove("app.message.copy")
            .unwrap_or_default();
        let paste = resolved
            .keys
            .shift_remove("app.clipboard.pasteImage")
            .unwrap_or_default();
        move |input| {
            if pi_rs_tui::keys::is_key_release_data(input) {
                return false;
            }
            if paste.iter().any(|key| matches_key_data(input, key)) {
                if let Some(clipboard) = get_native_clipboard()
                    && let Ok(ClipboardRead::Data(text)) = clipboard.get_text()
                {
                    let text: String = text
                        .chars()
                        .filter(|c| !c.is_control() || matches!(c, '\t' | '\n'))
                        .collect();
                    editor
                        .borrow_mut()
                        .handle_input(&format!("\x1b[200~{text}\x1b[201~"));
                }
                return true;
            }
            if !keys.iter().any(|key| matches_key_data(input, key)) {
                return false;
            }
            let mut grant = state.borrow_mut();
            if !grant.answer.is_empty() && grant.answer.len() <= 75_000 {
                grant.pending = Some(format!(
                    "\x1b]52;c;{}\x1b\\",
                    STANDARD.encode(grant.answer.as_bytes())
                ));
            }
            true
        }
    }
}
