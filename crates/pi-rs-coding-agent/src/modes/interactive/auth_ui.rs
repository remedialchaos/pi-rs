use async_trait::async_trait;
use pi_rs_agent::CancellationToken;
use pi_rs_ai::{Error, Result};
use tokio::sync::oneshot;

use crate::{
    cli::auth_terminal::LoginTerminal,
    core::extensions::types::{ExtensionUiHost, UiAnswer, UiDialog, UiDialogOptions},
    utils::open_browser::open_web_url,
};

use super::{UiEvent, extension_ui::UiHost};

pub(super) struct LoginUi(pub UiHost);

#[async_trait]
impl LoginTerminal for LoginUi {
    fn interactive(&self) -> bool {
        true
    }
    fn write(&self, line: &str) {
        let _ = self
            .0
            .events
            .send(UiEvent::Notice(line.to_owned(), "muted"));
    }
    fn error(&self, line: &str) {
        let _ = self
            .0
            .events
            .send(UiEvent::Notice(line.to_owned(), "error"));
    }
    fn open_url(&self, url: &str) {
        open_web_url(url);
    }
    async fn prompt(
        &self,
        message: &str,
        secret: bool,
        signal: &CancellationToken,
    ) -> Result<String> {
        if signal.is_cancelled() {
            return Err(Error::Aborted);
        }
        if secret {
            let (reply, receive) = oneshot::channel();
            self.0
                .events
                .send(UiEvent::SecretDialog(message.to_owned(), reply))
                .map_err(|_| Error::Aborted)?;
            return tokio::select! {
                biased;
                () = signal.cancelled() => Err(Error::Aborted),
                answer = receive => match answer { Ok(Some(UiAnswer::Text(value))) => Ok(value), _ => Err(Error::Aborted) },
            };
        }
        let options = UiDialogOptions {
            cancel: Some(signal.clone()),
            timeout_ms: None,
            blocked_kind: None,
        };
        match self
            .0
            .dialog(UiDialog::Input(message.to_owned(), None), options)
            .await
        {
            Some(UiAnswer::Text(text)) => Ok(text),
            _ => Err(Error::Aborted),
        }
    }
    async fn settle(&self) {
        // No terminal worker to join: dropping the response receiver closes the overlay.
    }
}
