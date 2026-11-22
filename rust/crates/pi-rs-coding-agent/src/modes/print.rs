use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};

use pi_rs_agent::AgentMessage;
use pi_rs_ai::{AssistantContent, ImageContent, StopReason};
use serde::Serialize;

use crate::core::{
    agent_session::{AgentSession, PromptOptions},
    agent_session_runtime::AgentSessionRuntime,
    event_bus::ExtensionEvent,
    extensions::types::ExtensionMode,
};

use super::json_event::to_json_event;

#[derive(Default)]
pub struct PrintOptions {
    pub json: bool,
    pub messages: Vec<String>,
    pub initial_message: Option<String>,
    pub initial_images: Vec<ImageContent>,
}

pub async fn run(runtime: &mut AgentSessionRuntime, options: PrintOptions) -> io::Result<u8> {
    run_inner(runtime, options, io::stdout(), super::termination_signal()?).await
}

/// Use one writer for the header, event stream and final text. Synchronous
/// writes naturally apply backpressure to the session event callback.
pub async fn run_with<W: Write + Send + 'static>(
    runtime: &mut AgentSessionRuntime,
    options: PrintOptions,
    output: W,
) -> io::Result<u8> {
    run_inner(runtime, options, output, std::future::pending()).await
}

async fn run_inner<W: Write + Send + 'static>(
    runtime: &mut AgentSessionRuntime,
    options: PrintOptions,
    output: W,
    terminate: impl Future<Output = u8>,
) -> io::Result<u8> {
    let session = runtime.session().clone();
    let mode = if options.json {
        ExtensionMode::Json
    } else {
        ExtensionMode::Print
    };
    let writer = Arc::new(Mutex::new(Output {
        writer: output,
        error: None,
    }));
    let id = if options.json {
        if let Some(disk) = session.session_manager().disk() {
            let mut output = writer.lock().unwrap_or_else(|error| error.into_inner());
            output.json(disk.lock().get_header())?;
        }
        let writer = writer.clone();
        Some(session.subscribe(Arc::new(move |event| {
            let mut output = writer.lock().unwrap_or_else(|error| error.into_inner());
            if output.error.is_none() {
                output.error = to_json_event(event)
                    .map_err(io::Error::from)
                    .and_then(|event| output.json(&event))
                    .err();
            }
        })))
    } else {
        None
    };
    if let Some(runner) = runtime.services().extensions.native() {
        runner.set_ui(mode, false);
        for error in runner
            .emit(
                ExtensionEvent::SessionStart,
                serde_json::json!({"type": "session_start", "reason": "startup"}),
            )
            .await
        {
            eprintln!("Extension error: {error}");
        }
    }
    let json = options.json;
    let result = tokio::select! {
        result = run_prompts(runtime, options) => Ok(result),
        code = terminate => Err(code),
    };
    let exit = match result {
        Ok(Ok(())) if json => Ok(0),
        Ok(Ok(())) => final_text(
            &session,
            &mut writer
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .writer,
        ),
        Ok(Err(error)) => {
            eprintln!("{error}");
            Ok(1)
        }
        Err(code) => Ok(code),
    };
    if let Some(id) = id {
        session.unsubscribe(id);
    }
    if let Some(runner) = runtime.services().extensions.native() {
        runner
            .emit(
                ExtensionEvent::SessionShutdown,
                serde_json::json!({"type": "session_shutdown", "reason": "quit"}),
            )
            .await;
        runner.invalidate();
    }
    runtime.dispose().await;
    let mut writer = writer.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(error) = writer.error.take() {
        return Err(error);
    }
    writer.writer.flush()?;
    exit
}

struct Output<W> {
    writer: W,
    error: Option<io::Error>,
}

impl<W: Write> Output<W> {
    fn json(&mut self, value: &impl Serialize) -> io::Result<()> {
        serde_json::to_writer(&mut self.writer, value)?;
        self.writer.write_all(b"\n")
    }
}

async fn run_prompts(
    runtime: &AgentSessionRuntime,
    options: PrintOptions,
) -> crate::core::agent_session::Result<()> {
    let first = options
        .initial_message
        .filter(|message| !message.is_empty())
        .map(|message| (message, options.initial_images));
    let messages = first.into_iter().chain(
        options
            .messages
            .into_iter()
            .map(|message| (message, Vec::new())),
    );
    let extensions = &runtime.services().extensions;
    for (mut text, mut images) in messages {
        if let Some(runner) = extensions.native()
            && runner.has_handlers(ExtensionEvent::Input)
        {
            let wire_images = (!images.is_empty()).then(|| serde_json::json!(images));
            let mut result = runner
                .emit_input(&text, wire_images, "interactive", None)
                .await;
            if result["action"] == "handled" {
                continue;
            }
            if let Some(updated) = result["text"].as_str() {
                text = updated.to_owned();
            }
            if let Some(updated) = result
                .as_object_mut()
                .and_then(|fields| fields.shift_remove("images"))
            {
                images = serde_json::from_value(updated).map_err(|error| {
                    crate::core::agent_session::SessionError::Extension(error.to_string())
                })?;
            }
        }
        if text.starts_with('/')
            && let Some(resources) = extensions.resources()
        {
            text = crate::core::prompt_templates::expand_prompt_template(&text, &resources.prompts);
        }
        runtime
            .session()
            .prompt(
                text,
                PromptOptions {
                    images,
                    expand_prompt_templates: true,
                    ..PromptOptions::default()
                },
            )
            .await?;
    }
    Ok(())
}

fn final_text(session: &AgentSession, output: &mut impl Write) -> io::Result<u8> {
    let messages = session.messages();
    let Some(AgentMessage::Assistant(message)) = messages.last() else {
        return Ok(0);
    };
    if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
        match message
            .error_message
            .as_deref()
            .filter(|message| !message.is_empty())
        {
            Some(message) => eprintln!("{message}"),
            None if message.stop_reason == StopReason::Aborted => eprintln!("Request aborted"),
            None => eprintln!("Request error"),
        }
        return Ok(1);
    }
    for content in &message.content {
        if let AssistantContent::Text(text) = content {
            writeln!(output, "{}", text.text)?;
        }
    }
    if message.stop_reason == StopReason::Length {
        eprintln!("Response was truncated: the model reached its output token limit.");
        return Ok(1);
    }
    Ok(0)
}
