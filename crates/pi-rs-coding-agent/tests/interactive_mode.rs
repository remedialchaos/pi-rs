#![forbid(unsafe_code)]

use std::{
    collections::VecDeque,
    io,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use pi_rs_agent::{Agent, AgentInitialState, AgentOptions, models_stream_fn};
use pi_rs_ai::{
    Models, StopReason,
    faux::{
        FauxHandle, FauxModelDefinition, FauxOptions, faux_assistant_message, faux_provider,
        faux_text, faux_tool_call,
    },
};
use pi_rs_coding_agent::{
    core::{
        agent_session::{
            AgentSession, AgentSessionConfig, ExtensionRunner as SessionExtensions,
            InMemoryModelRegistry, InMemorySettings, SessionError, SessionManager as SessionStore,
            SessionReason, StaticAuth, TailCompactor,
        },
        agent_session_runtime::{
            AgentSessionRuntime, CreateAgentSessionRuntimeFactory, CreateAgentSessionRuntimeResult,
            RuntimeServices, RuntimeTarget, create_agent_session_runtime,
        },
        agent_session_services::DiskSession,
        event_bus::EventBus,
        extensions::{
            runner::{ExtensionRunner, command, shortcut},
            types::{
                Extension, ExtensionHost, ExtensionInitContext, ExtensionRegistration,
                UiWidgetPlacement,
            },
        },
        session_manager::SessionManager,
        tools::wrappers::ReadTool,
    },
    modes::interactive::{InteractiveOptions, run_with_terminal},
};
use pi_rs_tui::{Terminal, TuiMode, strip_terminal_sequences, terminal::TerminalEvent};
use serde_json::{Map, Value, json};
use tokio::{sync::Notify, time::timeout};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[derive(Default)]
struct TerminalData {
    frames: Vec<String>,
    input: VecDeque<String>,
    active: bool,
}

#[derive(Clone, Default)]
struct VirtualTerminal {
    data: Arc<Mutex<TerminalData>>,
    changed: Arc<Notify>,
}

impl VirtualTerminal {
    fn with<T>(&self, f: impl FnOnce(&mut TerminalData) -> T) -> T {
        f(&mut self.data.lock().unwrap_or_else(|error| error.into_inner()))
    }
    fn type_text(&self, text: &str) {
        self.with(|data| data.input.extend(text.chars().map(|ch| ch.to_string())));
    }
    fn key(&self, key: &str) {
        self.with(|data| data.input.push_back(key.to_owned()));
    }
    fn submit(&self, text: &str) {
        self.type_text(text);
        self.key("\r");
    }
    fn offset(&self) -> usize {
        self.with(|data| data.frames.len())
    }
    async fn wait_for(&self, offset: usize, needle: &str) -> TestResult {
        self.wait_for_frame(offset, needle, |_| true).await
    }
    async fn wait_for_frame(
        &self,
        offset: usize,
        needle: &str,
        accepts: impl Fn(&str) -> bool,
    ) -> TestResult {
        timeout(Duration::from_secs(4), async {
            loop {
                let notified = self.changed.notified();
                if self.with(|data| {
                    data.frames[offset..].iter().any(|frame| {
                        let text = strip_terminal_sequences(frame);
                        text.contains(needle) && accepts(&text)
                    })
                }) {
                    break;
                }
                notified.await;
            }
        })
        .await
        .map_err(|_| {
            io::Error::other(format!(
                "missing terminal output {needle:?}: {}",
                self.with(|data| strip_terminal_sequences(
                    &data
                        .frames
                        .iter()
                        .rev()
                        .take(5)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("\n")
                ))
            ))
        })?;
        Ok(())
    }
}

impl Terminal for VirtualTerminal {
    fn width(&self) -> usize {
        100
    }
    fn height(&self) -> usize {
        28
    }
    fn write(&mut self, text: &str) {
        self.with(|data| data.frames.push(text.to_owned()));
        self.changed.notify_one();
    }
    fn is_tty(&self) -> bool {
        false
    }
    fn read_available(&mut self) -> Vec<u8> {
        Vec::new()
    }
    fn poll_input(&mut self, _timeout: Duration) -> io::Result<Vec<TerminalEvent>> {
        Ok(self.with(|data| {
            data.input
                .pop_front()
                .map(TerminalEvent::Input)
                .into_iter()
                .collect()
        }))
    }
    fn start_session(&mut self) -> io::Result<()> {
        self.with(|data| data.active = true);
        Ok(())
    }
    fn stop_session(&mut self) -> io::Result<()> {
        self.with(|data| data.active = false);
        Ok(())
    }
}

struct NativeExtensions(Arc<ExtensionRunner>);

impl SessionExtensions for NativeExtensions {
    fn emit(&self, _event: &Value) -> Value {
        Value::Null
    }
    fn native(&self) -> Option<&Arc<ExtensionRunner>> {
        Some(&self.0)
    }
}

struct DialogExtension(ExtensionRegistration);

#[async_trait]
impl Extension for DialogExtension {
    fn name(&self) -> &str {
        "<test:interactive>"
    }
    fn registration(&self) -> &ExtensionRegistration {
        &self.0
    }
    fn set_registration(&mut self, registration: ExtensionRegistration) {
        self.0 = registration;
    }
    async fn init(&mut self, context: &mut ExtensionInitContext<'_>) -> Result<(), String> {
        context.register_shortcut_handler(
            shortcut("f2", self.name()),
            Arc::new(|_, ctx| {
                Box::pin(async move {
                    ctx.base.ui.notify("shortcut reached", "info");
                    Ok(())
                })
            }),
        )?;
        context.register_command(command(
            "dialogs",
            Arc::new(|_, ctx| {
                Box::pin(async move {
                    let ui = &ctx.base.ui;
                    let selection = ui
                        .select("Pick a fruit", vec!["apple".into(), "pear".into()])
                        .await;
                    let confirmed = ui.confirm("Confirm fruit", "Use selection?").await;
                    let input = ui
                        .input("Enter greeting", Some("Greeting placeholder".into()))
                        .await;
                    let editor = ui
                        .editor("Edit greeting", Some("draft greeting".into()))
                        .await;
                    ui.set_widget(
                        "fruit",
                        Some(vec![format!(
                            "widget:{}",
                            selection.as_deref().unwrap_or("cancelled")
                        )]),
                        Some(UiWidgetPlacement::AboveEditor),
                    );
                    ui.set_status("fruit", Some(format!("confirmed:{confirmed}")));
                    ui.set_title("interactive test title");
                    ui.set_editor_text("editor restored");
                    ui.notify(
                        format!(
                            "Received: {} / {}",
                            input.as_deref().unwrap_or("cancelled"),
                            editor.as_deref().unwrap_or("cancelled")
                        ),
                        "info",
                    );
                    Ok(())
                })
            }),
        ))
    }
}

async fn runtime(
    cwd: &Path,
    agent_dir: &Path,
    faux: &FauxHandle,
) -> TestResult<AgentSessionRuntime> {
    let catalog = Models::default();
    catalog.set_provider(faux.provider.clone());
    let models = faux.models.clone();
    let factory: CreateAgentSessionRuntimeFactory = Arc::new(move |target| {
        let catalog = catalog.clone();
        let models = models.clone();
        Box::pin(async move {
            let auth = Arc::new(StaticAuth::allow_all());
            let settings = Arc::new(InMemorySettings::default());
            settings.set_compaction_enabled(false);
            let registry = Arc::new(InMemoryModelRegistry::new(models.to_vec(), auth.clone()));
            let compactor = Arc::new(TailCompactor { keep_recent: 1 });
            let bus = Arc::new(EventBus::new());
            let runner = Arc::new(ExtensionRunner::new(
                Vec::new(),
                Arc::new(ExtensionHost::new(bus.clone())),
                bus,
                &target.cwd,
            ));
            let loaded = runner
                .load_extensions(&[Arc::new(|| {
                    Ok(Box::new(DialogExtension(ExtensionRegistration::new(
                        "<test:interactive>",
                    ))))
                })])
                .await;
            if let Some(error) = loaded.errors.first() {
                return Err(SessionError::Extension(error.error.clone()));
            }
            let extensions = Arc::new(NativeExtensions(runner));
            let agent = Agent::new(AgentOptions {
                initial_state: AgentInitialState {
                    model: models.first().cloned(),
                    messages: target.session_manager.messages(),
                    ..Default::default()
                },
                stream_fn: Some(models_stream_fn(catalog)),
                ..Default::default()
            })?;
            agent.set_tools(vec![Arc::new(ReadTool::new(target.cwd.clone()))]);
            let session = AgentSession::new(AgentSessionConfig {
                agent,
                session_manager: target.session_manager,
                settings: settings.clone(),
                auth: auth.clone(),
                models: registry.clone(),
                compactor: compactor.clone(),
                extensions: extensions.clone(),
                cwd: target.cwd.clone(),
                retry_prompt: None,
            })?;
            Ok(CreateAgentSessionRuntimeResult {
                session,
                services: RuntimeServices {
                    cwd: target.cwd,
                    agent_dir: target.agent_dir,
                    settings,
                    auth,
                    models: registry,
                    compactor,
                    extensions,
                },
                diagnostics: Vec::new(),
                model_fallback_message: None,
            })
        })
    });
    let manager = Arc::new(DiskSession::new(SessionManager::in_memory_at(cwd)));
    Ok(create_agent_session_runtime(
        factory,
        RuntimeTarget {
            cwd: manager.cwd(),
            agent_dir: Some(agent_dir.display().to_string()),
            session_manager: manager,
            reason: SessionReason::Startup,
        },
    )
    .await?)
}

#[tokio::test]
async fn fullscreen_turn_and_extension_dialogs_share_the_live_terminal() -> TestResult {
    let dir = tempfile::tempdir()?;
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir)?;
    let mut contents = "visible tool preview\n".repeat(12);
    contents.push_str("streamed tool contents");
    std::fs::write(dir.path().join("fixture.txt"), contents)?;
    let faux = faux_provider(FauxOptions {
        token_size: Some((3, 3)),
        tokens_per_second: Some(38.),
        ..Default::default()
    });
    let mut call = faux_assistant_message(vec![faux_tool_call(
        "read",
        Map::from_iter([("path".to_owned(), json!("fixture.txt"))]),
        Some("read-1".into()),
    )]);
    call.stop_reason = StopReason::ToolUse;
    faux.set_responses([
        call.into(),
        faux_assistant_message(vec![faux_text(
            "Visible streamed response follows the tool result.",
        )])
        .into(),
    ]);
    let runtime = runtime(dir.path(), &agent_dir, &faux).await?;
    let terminal = VirtualTerminal::default();
    let options = InteractiveOptions {
        agent_dir,
        tui_mode: Some(TuiMode::Fullscreen),
        offline: true,
        ..Default::default()
    };
    let server = run_with_terminal(runtime, options, terminal.clone());
    let client = async {
        terminal.wait_for(0, "/ for commands").await?;
        let start = terminal.offset();
        terminal.submit("Read the fixture");
        terminal.wait_for(start, "Read the fixture").await?;
        terminal.wait_for(start, "fixture.txt").await?;
        terminal.wait_for(start, "visible tool preview").await?;
        terminal
            .wait_for_frame(start, "Visible streamed response", |frame| {
                !frame.contains("follows the tool result.")
            })
            .await?;
        terminal.wait_for(start, "follows the tool result.").await?;
        let start = terminal.offset();
        terminal.key("\x0f");
        terminal.wait_for(start, "streamed tool contents").await?;
        terminal.key("\x1bOQ");
        terminal.wait_for(start, "shortcut reached").await?;
        terminal.submit("/dialogs");
        terminal.wait_for(start, "Pick a fruit").await?;
        terminal.key("\x1b[B");
        terminal.key("\r");
        terminal.wait_for(start, "Confirm fruit").await?;
        terminal.key("\x1b[A");
        terminal.key("\r");
        terminal.wait_for(start, "Greeting placeholder").await?;
        terminal.submit("hello");
        terminal.wait_for(start, "draft greeting").await?;
        terminal.key("\r");
        terminal.wait_for(start, "widget:pear").await?;
        terminal.wait_for(start, "confirmed:true").await?;
        terminal
            .wait_for(start, "Received: hello / draft greeting")
            .await?;
        terminal.wait_for(start, "editor restored").await?;
        assert!(terminal.with(|data| {
            data.frames[start..]
                .iter()
                .any(|frame| frame.contains("\x1b]0;interactive test title\x07"))
        }));
        terminal.key("\x03");
        terminal.submit("/quit");
        Ok::<_, Box<dyn std::error::Error>>(())
    };
    let code = timeout(Duration::from_secs(18), async {
        tokio::try_join!(
            async { server.await.map_err(Box::<dyn std::error::Error>::from) },
            client
        )
        .map(|(code, ())| code)
    })
    .await??;
    assert_eq!(code, 0);
    assert!(!terminal.with(|data| data.active));
    assert_eq!(faux.state.call_count(), 2);
    Ok(())
}

#[tokio::test]
async fn regular_screen_selects_models_and_persists_theme_through_reloads() -> TestResult {
    let dir = tempfile::tempdir()?;
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir)?;
    let faux = faux_provider(FauxOptions {
        models: vec![
            FauxModelDefinition::default(),
            FauxModelDefinition {
                id: "reasoning-model".into(),
                reasoning: true,
                ..Default::default()
            },
        ],
        ..Default::default()
    });
    faux.set_responses([faux_assistant_message(vec![faux_text("new model answered")]).into()]);
    let runtime = runtime(dir.path(), &agent_dir, &faux).await?;
    let terminal = VirtualTerminal::default();
    let options = InteractiveOptions {
        agent_dir: agent_dir.clone(),
        tui_mode: Some(TuiMode::Regular),
        offline: true,
        ..Default::default()
    };
    let server = run_with_terminal(runtime, options, terminal.clone());
    let client = async {
        terminal.wait_for(0, "/ for commands").await?;
        let autocomplete = terminal.offset();
        terminal.type_text("/mod");
        terminal
            .wait_for(autocomplete, "Select model (opens selector UI)")
            .await?;
        terminal.key("\x1b");
        terminal.key("\x15");
        let start = terminal.offset();
        terminal.submit("/model reasoning-model");
        terminal
            .wait_for(start, "Model: faux/reasoning-model")
            .await?;
        terminal.submit("/thinking low");
        terminal.wait_for(start, "Thinking level: low").await?;
        terminal.wait_for(start, "reasoning-model • low").await?;
        terminal.submit("/theme light");
        terminal.submit("run after model change");
        terminal.wait_for(start, "new model answered").await?;
        terminal.submit("/compact");
        terminal.wait_for(start, "Compacted").await?;
        terminal.submit("/reload");
        terminal.wait_for(start, "Reloaded keybindings").await?;
        terminal.submit("/new");
        terminal.wait_for(start, "Started a new session").await?;
        terminal.submit("/quit");
        Ok::<_, Box<dyn std::error::Error>>(())
    };
    let code = timeout(Duration::from_secs(18), async {
        tokio::try_join!(
            async { server.await.map_err(Box::<dyn std::error::Error>::from) },
            client
        )
        .map(|(code, ())| code)
    })
    .await??;
    assert_eq!(code, 0);
    assert!(!terminal.with(|data| data.active));
    let persisted: Value =
        serde_json::from_slice(&std::fs::read(agent_dir.join("settings.json"))?)?;
    assert_eq!(persisted["theme"], "light");
    assert_eq!(persisted["defaultThinkingLevel"], "low");
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn executable_starts_and_quits_on_a_real_pty() -> TestResult {
    let dir = tempfile::tempdir()?;
    let frame = pty(
        dir.path(),
        "--offline --no-session --no-extensions --tui-mode regular",
        &[("/ for commands", b"/quit\r")],
    )
    .await?;
    assert!(frame.contains("\x1b[?2004h") && frame.contains("\x1b[?2004l"));
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn config_tui_disables_a_prompt_in_runtime_resources() -> TestResult {
    let dir = tempfile::tempdir()?;
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(agent.join("prompts"))?;
    std::fs::write(
        agent.join("prompts/config-probe.md"),
        "Disabled description.\n",
    )?;
    std::fs::write(
        agent.join("prompts/config-control.md"),
        "Enabled description.\n",
    )?;
    pty(
        dir.path(),
        "config",
        &[
            ("config-probe.md", b"config-probe "),
            ("[ ] config-probe.md", b"\x1b"),
        ],
    )
    .await?;
    let settings: Value = serde_json::from_slice(&std::fs::read(agent.join("settings.json"))?)?;
    assert!(
        settings["prompts"]
            .as_array()
            .is_some_and(|rules| rules.iter().any(|rule| rule
                .as_str()
                .is_some_and(|rule| rule.starts_with('-') && rule.ends_with("config-probe.md"))))
    );
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_pi"))
        .args([
            "--mode",
            "rpc",
            "--offline",
            "--no-session",
            "--no-extensions",
        ])
        .env_clear()
        .env("HOME", dir.path())
        .env("PI_CODING_AGENT_DIR", &agent)
        .env("OPENAI_API_KEY", "offline-fixture")
        .current_dir(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or("RPC stdin missing")?;
    stdin.write_all(b"{\"type\":\"get_commands\"}\n").await?;
    drop(stdin);
    let output = timeout(Duration::from_secs(10), child.wait_with_output()).await??;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    let response: Value = serde_json::from_str(stdout.trim())?;
    let commands = response["data"]["commands"]
        .as_array()
        .ok_or("RPC commands missing")?;
    assert!(
        commands
            .iter()
            .any(|command| command["name"] == "config-control")
    );
    assert!(
        !commands
            .iter()
            .any(|command| command["name"] == "config-probe")
    );
    Ok(())
}

#[cfg(target_os = "linux")]
async fn pty(cwd: &Path, args: &str, steps: &[(&str, &[u8])]) -> TestResult<String> {
    use std::process::Stdio;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut child = tokio::process::Command::new("script")
        .args([
            "-qefc",
            &format!("exec \"$PI_TEST_BIN\" {args}"),
            "/dev/null",
        ])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", cwd)
        .env("PI_TEST_BIN", env!("CARGO_BIN_EXE_pi"))
        .env("PI_CODING_AGENT_DIR", cwd.join("agent"))
        .env("TERM", "xterm-256color")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut input = child.stdin.take().ok_or("script stdin missing")?;
    let mut output = child.stdout.take().ok_or("script stdout missing")?;
    let mut bytes = Vec::new();
    let result = timeout(Duration::from_secs(15), async {
        let mut chunk = [0; 4096];
        for &(expected, keys) in steps {
            while !strip_terminal_sequences(&String::from_utf8_lossy(&bytes)).contains(expected) {
                let read = output.read(&mut chunk).await?;
                if read == 0 {
                    return Err(io::Error::other(format!("pi exited before {expected:?}")));
                }
                bytes.extend_from_slice(&chunk[..read]);
            }
            input.write_all(keys).await?;
        }
        let _ = output.read_to_end(&mut bytes).await?;
        Ok::<_, io::Error>(())
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        error => {
            return Err(format!(
                "{args}: {error:?}\n{}",
                strip_terminal_sequences(&String::from_utf8_lossy(&bytes))
            )
            .into());
        }
    }
    drop(input);
    let status = timeout(Duration::from_secs(4), child.wait()).await??;
    let frame = String::from_utf8_lossy(&bytes);
    assert!(status.success(), "{status}: {frame}");
    assert!(!frame.contains("scheduled for Milestone"));
    Ok(frame.into_owned())
}
