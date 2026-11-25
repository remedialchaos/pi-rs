use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::core::{
    agent_session::{ForkPosition, SessionReason},
    event_bus::ExtensionEvent,
    extensions::{
        runner::ExtensionRunner,
        types::{
            ContextUsage, ExtensionCommandContext, ExtensionContext, ExtensionMode, ExtensionUi,
        },
    },
};

use super::{BoundSession, Reply, Server, Transition, TransitionMessage, session};

impl Server<'_> {
    pub(super) async fn internal_transition(
        &self,
        transition: Transition,
        response: oneshot::Sender<Reply>,
    ) {
        let _ = response.send(self.transition(transition).await);
    }

    pub(super) async fn transition(&self, transition: Transition) -> Reply {
        let mut runtime = self.runtime.lock().await;
        let bound = BoundSession::new(&runtime);
        let session = &bound.session;
        let native = bound.extensions.native();
        let previous_file = session.session_file();
        let (new_manager, reason, selected_text) = match transition {
            Transition::New(parent) => {
                if before_switch(native, "new", None).await {
                    return cancelled();
                }
                (
                    session::new_session(session, parent.as_deref())?,
                    SessionReason::New,
                    None,
                )
            }
            Transition::Switch(path) => {
                if before_switch(native, "resume", Some(&path)).await {
                    return cancelled();
                }
                (
                    session::switch_session(session, &path)?,
                    SessionReason::Resume,
                    None,
                )
            }
            fork @ (Transition::Fork(..) | Transition::Clone) => {
                let (id, position) = match fork {
                    Transition::Fork(id, position) => (id, position),
                    _ => (
                        session::leaf_id(session)
                            .ok_or("Cannot clone session: no current entry selected")?,
                        ForkPosition::At,
                    ),
                };
                if let Some(native) = native {
                    let position = match position {
                        ForkPosition::At => "at",
                        ForkPosition::Before => "before",
                    };
                    let event = json!({ "type": "session_before_fork", "entryId": id, "position": position });
                    if native
                        .bus
                        .emit_first(ExtensionEvent::SessionBeforeFork, event, is_cancelled)
                        .await
                        .is_some()
                    {
                        return cancelled();
                    }
                }
                // Settle before copying entries so the old branch is fully persisted.
                session.abort().await;
                let (manager, text) = session::fork_session(session, &id, position)?;
                (manager, SessionReason::Fork, text)
            }
            Transition::Reload => (session.session_manager(), SessionReason::Reload, None),
        };
        if let Some(native) = native {
            session.abort().await;
            let mut event = json!({ "type": "session_shutdown", "reason": reason.as_str() });
            if let Some(path) = new_manager.session_file() {
                event["targetSessionFile"] = json!(path);
            }
            native.emit(ExtensionEvent::SessionShutdown, event).await;
            native.invalidate();
        }
        self.abort_bash();
        runtime
            .replace_session(new_manager, reason)
            .await
            .map_err(|error| error.to_string())?;
        self.bind(&runtime, reason, previous_file).await;
        let mut data = json!({ "cancelled": false });
        if let Some(text) = selected_text {
            data["text"] = json!(text);
        }
        Ok(Some(data))
    }

    pub(super) fn command_context(
        &self,
        bound: &BoundSession,
        native: &ExtensionRunner,
    ) -> ExtensionCommandContext {
        let session = &bound.session;
        let idle = session.clone();
        let pending = session.clone();
        let usage = session.clone();
        let system = session.clone();
        let abort = session.clone();
        let shutdown = self.shutdown.clone();
        let compact = session.clone();
        let wait = session.clone();
        let navigate = session.clone();
        let new_tx = self.transitions.clone();
        let fork_tx = self.transitions.clone();
        let switch_tx = self.transitions.clone();
        let reload_tx = self.transitions.clone();
        ExtensionCommandContext {
            base: ExtensionContext {
                host: native.host.clone(),
                cwd: session.cwd(),
                mode: ExtensionMode::Rpc,
                ui: ExtensionUi::with_host(ExtensionMode::Rpc, self.ui.clone()),
                bus: Some(native.bus.clone()),
                is_idle: Arc::new(move || idle.is_idle()),
                is_project_trusted: Arc::new(|| false),
                has_pending_messages: Arc::new(move || {
                    !pending.peek_steering().is_empty() || !pending.peek_follow_up().is_empty()
                }),
                get_context_usage: Arc::new(move || {
                    let usage = usage.stats().context_usage?;
                    Some(ContextUsage {
                        tokens: usage.tokens.and_then(|tokens| i64::try_from(tokens).ok()),
                        context_window: i64::try_from(usage.context_window).ok()?,
                        percent: usage.percent,
                    })
                }),
                get_system_prompt: Arc::new(move || system.system_prompt()),
                abort: Arc::new(move || {
                    let session = abort.clone();
                    tokio::spawn(async move { session.abort().await });
                }),
                shutdown: Arc::new(move || shutdown.notify_one()),
                compact: Arc::new(move |options| {
                    let session = compact.clone();
                    tokio::spawn(async move {
                        let instructions = options
                            .as_ref()
                            .and_then(|value| value.get("customInstructions"))
                            .and_then(Value::as_str);
                        let _ = session.compact(instructions).await;
                    });
                }),
            },
            wait_for_idle: Arc::new(move || {
                let session = wait.clone();
                Box::pin(async move { session.wait_for_idle().await })
            }),
            new_session: Arc::new(move |options| {
                let parent = options
                    .as_ref()
                    .and_then(|options| options.get("parentSession"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                Box::pin(request_transition(new_tx.clone(), Transition::New(parent)))
            }),
            fork: Arc::new(move |id, options| {
                let position = options
                    .as_ref()
                    .and_then(|options| options.get("position"))
                    .and_then(Value::as_str);
                let position = if position == Some("at") {
                    ForkPosition::At
                } else {
                    ForkPosition::Before
                };
                let tx = fork_tx.clone();
                Box::pin(async move {
                    let data = request_transition(tx, Transition::Fork(id, position)).await?;
                    Ok(json!({ "cancelled": data["cancelled"] }))
                })
            }),
            navigate_tree: Arc::new(move |id, _options| {
                let session = navigate.clone();
                Box::pin(async move {
                    session
                        .navigate_tree(&id)
                        .await
                        .map_err(|error| error.to_string())?;
                    Ok(json!({ "cancelled": false }))
                })
            }),
            switch_session: Arc::new(move |path, _options| {
                Box::pin(request_transition(
                    switch_tx.clone(),
                    Transition::Switch(path),
                ))
            }),
            reload: Arc::new(move || {
                let tx = reload_tx.clone();
                Box::pin(
                    async move { request_transition(tx, Transition::Reload).await.map(|_| ()) },
                )
            }),
        }
    }
}

fn is_cancelled(result: &Value) -> bool {
    result.get("cancel").and_then(Value::as_bool) == Some(true)
}

fn cancelled() -> Reply {
    Ok(Some(json!({ "cancelled": true })))
}

async fn before_switch(
    native: Option<&Arc<ExtensionRunner>>,
    reason: &str,
    path: Option<&str>,
) -> bool {
    let Some(native) = native else { return false };
    let mut event = json!({ "type": "session_before_switch", "reason": reason });
    if let Some(path) = path {
        event["targetSessionFile"] = json!(path);
    }
    native
        .emit_session_before_switch(event)
        .await
        .is_some_and(|value| is_cancelled(&value))
}

async fn request_transition(
    sender: mpsc::UnboundedSender<TransitionMessage>,
    transition: Transition,
) -> Result<Value, String> {
    let (response, receive) = oneshot::channel();
    sender
        .send((transition, response))
        .map_err(|_| "RPC session is closed".to_owned())?;
    receive
        .await
        .map_err(|_| "RPC session is closed".to_owned())??
        .ok_or_else(|| "Missing session transition result".to_owned())
}
