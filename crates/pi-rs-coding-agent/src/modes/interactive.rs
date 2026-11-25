//! Native interactive coding agent. Terminal components stay on the UI thread;
//! session and extension futures progress alongside renderer polling.

mod auth_ui;
mod chrome;
mod commands;
mod controller;
mod extension_ui;
mod screen;
pub mod selectors;
pub mod theme;
mod transcript;
pub mod utilities;

use std::{path::PathBuf, rc::Rc, time::Duration};

use futures_util::{StreamExt, future::LocalBoxFuture, stream::FuturesUnordered};
use pi_rs_tui::{Terminal, TerminalColorScheme, TerminalColors, TuiMode};
use tokio::sync::{mpsc, oneshot};

use crate::{
    cli::initial_message::InitialMessage,
    core::{
        agent_session::AgentSessionEvent,
        agent_session_runtime::{AgentSessionRuntime, RuntimeDiagnostic},
        bash_executor::BashOutput,
        extensions::types::{UiAnswer, UiDialog, UiUpdate},
        keybindings::AppKeybinding,
    },
};

mod frontend;
use controller::{Bound, Controller, Transition};
use frontend::Frontend;
use screen::TerminalHandle;

#[derive(Default)]
pub struct InteractiveOptions {
    pub agent_dir: PathBuf,
    pub tui_mode: Option<TuiMode>,
    pub theme: Option<String>,
    pub project_trust_override: Option<bool>,
    pub initial: InitialMessage,
    pub messages: Vec<String>,
    pub offline: bool,
    pub no_refresh: bool,
    pub startup_benchmark: bool,
}

type Work = FuturesUnordered<LocalBoxFuture<'static, ()>>;
type DialogAnswer = oneshot::Sender<Option<UiAnswer>>;

enum UiEvent {
    Submit(String),
    Key(AppKeybinding),
    Shortcut(String),
    Choice(commands::Choice),
    CloseSelector,
    Paste,
    Suspend,
    Quit,
    Bound(Bound, Vec<RuntimeDiagnostic>),
    Models(u64, Vec<pi_rs_ai::Model>),
    Session(u64, Box<AgentSessionEvent>),
    Restore(u64),
    Dialog(Option<u64>, UiDialog, DialogAnswer),
    SecretDialog(String, DialogAnswer),
    ExtensionUpdate(Option<u64>, UiUpdate),
    DialogFinished(u64, Option<UiAnswer>),
    Transition(
        Transition,
        Option<oneshot::Sender<Result<serde_json::Value, String>>>,
    ),
    TransitionFinished(Result<(), String>),
    Notice(String, &'static str),
    Colors(TerminalColors),
    ColorScheme(TerminalColorScheme),
    ThemeFile,
    BashChunk(u64, String),
    BashEnd(u64, Result<BashOutput, String>),
    EditorFinished(Result<Option<String>, String>),
    AuthFinished(u8),
}

/// Run the interactive mode on the process terminal.
pub async fn run(runtime: AgentSessionRuntime, options: InteractiveOptions) -> Result<u8, String> {
    let terminate = super::termination_signal().map_err(|error| error.to_string())?;
    drive(runtime, options, TerminalHandle::process(), terminate).await
}

/// Run the same interactive frontend on an injected terminal transport.
pub async fn run_with_terminal(
    runtime: AgentSessionRuntime,
    options: InteractiveOptions,
    terminal: impl Terminal + 'static,
) -> Result<u8, String> {
    drive(
        runtime,
        options,
        TerminalHandle::injected(terminal),
        std::future::pending(),
    )
    .await
}

async fn drive(
    runtime: AgentSessionRuntime,
    mut options: InteractiveOptions,
    terminal: TerminalHandle,
    terminate: impl Future<Output = u8>,
) -> Result<u8, String> {
    let (send, mut receive) = mpsc::unbounded_channel();
    let initial = std::mem::take(&mut options.initial);
    let messages = std::mem::take(&mut options.messages);
    let options = Rc::new(options);
    let controller = Controller::new(runtime, send.clone());
    let bound = controller.active();
    let mut ui = Frontend::new(terminal, options.clone(), bound.clone(), send)?;
    let mut work = Work::new();
    bound.refresh_catalog(
        &work,
        controller.events.clone(),
        options.offline || options.no_refresh || options.startup_benchmark,
    );
    let first = controller.clone();
    let benchmark = options.startup_benchmark;
    work.push(Box::pin(async move {
        first.bind(bound, "startup", None).await;
        if benchmark {
            let _ = first.events.send(UiEvent::Quit);
            return;
        }
        let images = initial.initial_images.unwrap_or_default();
        if initial.initial_message.is_some() || !images.is_empty() {
            first
                .prompt(
                    first.active(),
                    initial.initial_message.unwrap_or_default(),
                    images,
                    false,
                )
                .await;
        }
        for message in messages {
            first
                .prompt(first.active(), message, Vec::new(), false)
                .await;
        }
    }));
    let mut ticks = tokio::time::interval(Duration::from_millis(12));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut terminate = std::pin::pin!(terminate);
    let result = loop {
        tokio::select! {
            code = &mut terminate => break Ok(code),
            _ = ticks.tick() => {
                if let Err(error) = ui.step() { break Err(error.to_string()); }
            }
            _ = work.next(), if !work.is_empty() => {}
            Some(event) = receive.recv() => {
                if matches!(event, UiEvent::Quit) { break Ok(0); }
                if let Err(error) = ui.dispatch(event, &controller, &work) { ui.notice(error, "error"); }
            }
        }
    };
    ui.close_dialogs();
    receive.close();
    drop(receive);
    ui.cancel_bash();
    ui.cancel_auth();
    // Let running prompts observe cancellation while awaiting their settlement.
    let shutdown = controller.close();
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => break,
            _ = work.next(), if !work.is_empty() => {}
        }
    }
    let restore = ui.finish().map_err(|error| error.to_string());
    result.and_then(|code| restore.map(|()| code))
}
