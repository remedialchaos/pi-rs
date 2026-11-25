use std::{
    io::{self, Write},
    sync::{Arc, Mutex, RwLock},
};

use pi_rs_agent::{AgentEvent, models_stream_fn};
use pi_rs_ai::{
    AssistantMessageEvent, Models, StopReason,
    faux::{
        FauxOptions, FauxStep, faux_assistant_message, faux_provider, faux_text, faux_thinking,
        faux_tool_call,
    },
};
use pi_rs_coding_agent::{
    core::{
        agent_session::{
            AgentSessionEvent, InMemoryModelRegistry, InMemorySettings, NoopExtensionRunner,
            StaticAuth, TailCompactor,
        },
        agent_session_runtime::{AgentSessionRuntime, RuntimeServices},
        agent_session_services::DiskSession,
        sdk::{PiSdkOptions, create_sdk_session},
        session_manager::SessionManager,
    },
    modes::{
        json_event::to_json_event,
        print::{PrintOptions, run_with},
    },
};
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Default)]
struct Output(Arc<Mutex<Vec<u8>>>);

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .map_err(|error| io::Error::other(error.to_string()))?
            .write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Output {
    fn text(&self) -> TestResult<String> {
        Ok(String::from_utf8(
            self.0.lock().map_err(|error| error.to_string())?.clone(),
        )?)
    }
}

fn runtime(responses: impl IntoIterator<Item = FauxStep>) -> TestResult<AgentSessionRuntime> {
    let faux = faux_provider(FauxOptions::default());
    let model = faux.get_model(None).ok_or("no faux model")?.clone();
    faux.set_responses(responses);
    let models = Models::default();
    models.set_provider(faux.provider.clone());
    let auth = Arc::new(StaticAuth::allow_all());
    let registry = Arc::new(InMemoryModelRegistry::new(
        vec![model.clone()],
        auth.clone(),
    ));
    let settings = Arc::new(InMemorySettings::default());
    settings.set_compaction_enabled(false);
    settings.set_retry_enabled(false);
    let extensions = Arc::new(NoopExtensionRunner);
    let compactor = Arc::new(TailCompactor { keep_recent: 3 });
    let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
    let session = Arc::new(DiskSession::new(SessionManager::in_memory_at(&cwd)));
    let sdk = create_sdk_session(PiSdkOptions {
        cwd: cwd.clone(),
        model: Some(model),
        session_manager: Some(session),
        settings: Some(settings.clone()),
        auth: Some(auth.clone()),
        models: Some(registry.clone()),
        compactor: Some(compactor.clone()),
        extensions: Some(extensions.clone()),
        stream_fn: Some(models_stream_fn(models)),
        ..PiSdkOptions::default()
    })?;
    let services = RuntimeServices {
        cwd,
        agent_dir: None,
        settings,
        auth,
        models: registry,
        compactor,
        extensions,
    };
    Ok(AgentSessionRuntime::new(
        sdk.session().clone(),
        services,
        Arc::new(|_| {
            Box::pin(async { Err(pi_rs_coding_agent::core::agent_session::SessionError::Disposed) })
        }),
        Vec::new(),
        None,
    ))
}

#[tokio::test]
async fn print_emits_only_last_reply_text_and_keeps_truncated_output() -> TestResult {
    let mut truncated = faux_assistant_message(vec![
        faux_thinking("not stdout"),
        faux_text("one"),
        faux_text("two"),
    ]);
    truncated.stop_reason = StopReason::Length;
    let mut runtime = runtime([
        faux_assistant_message(vec![faux_text("not the last reply")]).into(),
        truncated.into(),
    ])?;
    let output = Output::default();
    let code = run_with(
        &mut runtime,
        PrintOptions {
            initial_message: Some("first question".into()),
            messages: vec!["second question".into()],
            ..PrintOptions::default()
        },
        output.clone(),
    )
    .await?;
    assert_eq!(code, 1);
    assert_eq!(output.text()?, "one\ntwo\n");
    Ok(())
}

#[tokio::test]
async fn json_has_header_deltas_and_final_message_even_when_provider_reports_length() -> TestResult
{
    let mut answer = faux_assistant_message(vec![faux_text("a\u{2028}b")]);
    answer.stop_reason = StopReason::Length;
    let mut runtime = runtime([answer.into()])?;
    let output = Output::default();
    let code = run_with(
        &mut runtime,
        PrintOptions {
            json: true,
            messages: vec!["prompt".into()],
            ..PrintOptions::default()
        },
        output.clone(),
    )
    .await?;
    assert_eq!(code, 0);
    let text = output.text()?;
    let events = text
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        events.first().map(|header| &header["type"]),
        Some(&json!("session"))
    );
    assert_eq!(
        events.first().map(|header| &header["version"]),
        Some(&json!(3))
    );
    let mut joined = String::new();
    for event in events
        .iter()
        .filter(|event| event["type"] == "message_update")
    {
        assert!(event.get("message").is_none());
        assert!(event["assistantMessageEvent"].get("partial").is_none());
        if event["assistantMessageEvent"]["type"] == "text_delta" {
            joined.push_str(
                event["assistantMessageEvent"]["delta"]
                    .as_str()
                    .ok_or("missing text delta")?,
            );
        }
    }
    assert_eq!(joined, "a\u{2028}b");
    let final_message = events
        .iter()
        .find(|event| event["type"] == "message_end" && event["message"]["role"] == "assistant")
        .ok_or("no final assistant message")?;
    assert_eq!(final_message["message"]["content"][0]["text"], "a\u{2028}b");
    assert_eq!(final_message["message"]["stopReason"], "length");
    Ok(())
}

#[test]
fn tool_start_exposes_id_and_name_without_copying_partial_message() -> TestResult {
    let partial = Arc::new(RwLock::new(faux_assistant_message(vec![faux_tool_call(
        "read",
        Default::default(),
        Some("call-1".into()),
    )])));
    let event = |index| {
        AgentSessionEvent::Core(AgentEvent::MessageUpdate {
            message: partial.clone(),
            assistant_message_event: AssistantMessageEvent::ToolCallStart {
                content_index: index,
                partial: partial.clone(),
            },
        })
    };
    let wire = to_json_event(&event(0))?;
    assert_eq!(
        wire["assistantMessageEvent"],
        json!({"type":"toolcall_start","contentIndex":0,"id":"call-1","toolName":"read"})
    );
    assert!(wire.get("usage").and_then(Value::as_object).is_some());
    assert!(to_json_event(&event(1)).is_err());
    Ok(())
}
