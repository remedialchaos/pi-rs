use std::{future::pending, time::Duration};

use futures_util::future::BoxFuture;
use tokio::sync::{mpsc::UnboundedSender, oneshot};

use crate::core::extensions::types::{
    ExtensionUiHost, UiAnswer, UiDialog, UiDialogOptions, UiUpdate,
};

use super::UiEvent;

#[derive(Clone)]
pub(super) struct UiHost {
    pub(super) generation: Option<u64>,
    pub(super) events: UnboundedSender<UiEvent>,
}

impl ExtensionUiHost for UiHost {
    fn dialog(
        &self,
        dialog: UiDialog,
        options: UiDialogOptions,
    ) -> BoxFuture<'static, Option<UiAnswer>> {
        let host = self.clone();
        Box::pin(async move {
            if options
                .cancel
                .as_ref()
                .is_some_and(pi_rs_agent::CancellationToken::is_cancelled)
            {
                return None;
            }
            let (answer, receive) = oneshot::channel();
            host.events
                .send(UiEvent::Dialog(
                    host.generation,
                    dialog,
                    options.blocked_kind,
                    answer,
                ))
                .ok()?;
            tokio::select! {
                biased;
                () = async {
                    if let Some(cancel) = options.cancel { cancel.cancelled().await; }
                    else { pending().await }
                } => None,
                response = receive => response.ok().flatten(),
                () = async {
                    if let Some(ms) = options.timeout_ms.filter(|ms| *ms != 0) {
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                    } else { pending().await }
                } => None,
            }
        })
    }

    fn update(&self, update: UiUpdate) {
        let _ = self
            .events
            .send(UiEvent::ExtensionUpdate(self.generation, update));
    }
}
