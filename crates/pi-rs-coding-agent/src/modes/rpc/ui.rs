use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::future::BoxFuture;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::core::extensions::types::{
    ExtensionUiHost, UiAnswer, UiDialog, UiDialogOptions, UiUpdate, UiWidgetPlacement,
};

use super::{
    Output, RpcExtensionUiRequest, RpcExtensionUiResponse, RpcUiMethod, RpcWidgetPlacement,
};

type Dialogs = Mutex<Option<HashMap<Uuid, oneshot::Sender<RpcExtensionUiResponse>>>>;

pub(super) struct RpcUi {
    output: Output,
    pending: Arc<Dialogs>,
}

impl RpcUi {
    pub(super) fn new(output: Output) -> Self {
        Self {
            output,
            pending: Arc::new(Mutex::new(Some(HashMap::new()))),
        }
    }

    pub(super) fn answer(&self, response: RpcExtensionUiResponse) {
        let Ok(id) = Uuid::parse_str(&response.id) else {
            return;
        };
        if let Some(sender) = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_mut()
            .and_then(|pending| pending.remove(&id))
        {
            let _ = sender.send(response);
        }
    }

    pub(super) fn close(&self) {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }

    fn emit(&self, id: Uuid, method: RpcUiMethod) {
        self.output.send(&RpcExtensionUiRequest {
            id: id.to_string(),
            method,
        });
    }
}

struct PendingDialog {
    pending: Arc<Dialogs>,
    id: Uuid,
}

impl Drop for PendingDialog {
    fn drop(&mut self) {
        if let Some(pending) = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_mut()
        {
            pending.remove(&self.id);
        }
    }
}

impl ExtensionUiHost for RpcUi {
    fn dialog(
        &self,
        dialog: UiDialog,
        options: UiDialogOptions,
    ) -> BoxFuture<'static, Option<UiAnswer>> {
        if options
            .cancel
            .as_ref()
            .is_some_and(pi_rs_agent::CancellationToken::is_cancelled)
        {
            return Box::pin(async { None });
        }
        let timeout = options.timeout_ms;
        let method = match dialog {
            UiDialog::Select(title, options) => RpcUiMethod::Select {
                title,
                options,
                timeout,
            },
            UiDialog::Confirm(title, message) => RpcUiMethod::Confirm {
                title,
                message,
                timeout,
            },
            UiDialog::Input(title, placeholder) => RpcUiMethod::Input {
                title,
                placeholder,
                timeout,
            },
            UiDialog::Editor(title, prefill) => RpcUiMethod::Editor { title, prefill },
        };
        let id = Uuid::new_v4();
        let (tx, rx) = oneshot::channel();
        {
            let mut guard = self
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let Some(pending) = guard.as_mut() else {
                return Box::pin(async { None });
            };
            pending.insert(id, tx);
        }
        let pending = PendingDialog {
            pending: self.pending.clone(),
            id,
        };
        let confirm = matches!(method, RpcUiMethod::Confirm { .. });
        self.emit(id, method);
        Box::pin(async move {
            let _pending = pending;
            let response = tokio::select! {
                response = rx => response.ok(),
                () = async {
                    match timeout.filter(|value| *value != 0) {
                        Some(timeout) => tokio::time::sleep(Duration::from_millis(timeout)).await,
                        None => std::future::pending().await,
                    }
                } => None,
                () = async {
                    match options.cancel {
                        Some(cancel) => cancel.cancelled().await,
                        None => std::future::pending().await,
                    }
                } => None,
            }?;
            if response.cancelled == Some(true) {
                None
            } else if confirm {
                response.confirmed.map(UiAnswer::Confirmed)
            } else {
                response.value.map(UiAnswer::Text)
            }
        })
    }

    fn update(&self, update: UiUpdate) {
        let method = match update {
            UiUpdate::Notify(message, notify_type) => RpcUiMethod::Notify {
                message,
                notify_type,
            },
            UiUpdate::Status(status_key, status_text) => RpcUiMethod::SetStatus {
                status_key,
                status_text,
            },
            UiUpdate::Widget(widget_key, widget_lines, placement) => RpcUiMethod::SetWidget {
                widget_key,
                widget_lines,
                widget_placement: placement.map(|placement| match placement {
                    UiWidgetPlacement::AboveEditor => RpcWidgetPlacement::AboveEditor,
                    UiWidgetPlacement::BelowEditor => RpcWidgetPlacement::BelowEditor,
                }),
            },
            UiUpdate::Title(title) => RpcUiMethod::SetTitle { title },
            UiUpdate::EditorText(text) => RpcUiMethod::SetEditorText { text },
        };
        self.emit(Uuid::new_v4(), method);
    }
}
