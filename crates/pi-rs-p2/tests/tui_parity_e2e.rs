//! Comprehensive live PTY E2E tests for Workstream 1.5 (`p2 tui` core overlays,
//! keybindings, shell/quiet-tools toggles, and interactive built-in plugin facets).

use std::{
    error::Error,
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use pi_rs_chord::{context::Context, host::Caller, node::workers::WorkerOptions};
use pi_rs_p2::{
    api::v1::p2_contracts::lookup,
    hosts::server::{P2Server, ServerOptions, start_server},
    plugins,
};
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

fn owner_caller() -> Caller {
    Caller {
        principal: Some(json!({"kind": "owner", "access": "local"})),
        incarnation: uuid::Uuid::new_v4().to_string(),
    }
}

async fn start_native_server(directory: &Path) -> TestResult<P2Server> {
    let mut options = ServerOptions::new(directory.to_owned());
    options.port = 0;
    options.fake = true;
    options.dev = false;
    plugins::register_builtins(&mut options.registry)?;
    let mut process = WorkerOptions::new(
        env!("CARGO_BIN_EXE_p2").into(),
        directory.join("worker-locks"),
    );
    process.args.push("--worker".into());
    options.worker_process = Some(process);
    Ok(start_server(options).await?)
}

fn wait_for_screen_text(
    captured: &Arc<Mutex<Vec<u8>>>,
    from_offset: usize,
    needle: &str,
    timeout: Duration,
) -> TestResult<usize> {
    let deadline = Instant::now() + timeout;
    loop {
        let (len, plain) = {
            let guard = captured.lock().unwrap_or_else(|p| p.into_inner());
            let slice = guard.get(from_offset..).unwrap_or(&[]);
            let raw = String::from_utf8_lossy(slice);
            (guard.len(), pi_rs_tui::strip_terminal_sequences(&raw))
        };
        if plain.contains(needle) {
            return Ok(len);
        }
        if Instant::now() >= deadline {
            let full = {
                let guard = captured.lock().unwrap_or_else(|p| p.into_inner());
                pi_rs_tui::strip_terminal_sequences(&String::from_utf8_lossy(&guard))
            };
            return Err(format!(
                "timed out waiting for {needle:?} in PTY output.\nCaptured plain text:\n{full}"
            )
            .into());
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn send_line(stdin: &mut impl Write, line: &str) -> TestResult {
    stdin.write_all(line.as_bytes())?;
    stdin.write_all(b"\r")?;
    stdin.flush()?;
    Ok(())
}

fn send_raw(stdin: &mut impl Write, bytes: &[u8]) -> TestResult {
    stdin.write_all(bytes)?;
    stdin.flush()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_p2_tui_core_overlays_shortcuts_and_plugin_facets() -> TestResult {
    if Command::new("script").arg("--version").output().is_err() {
        return Err("script(1) is required for PTY E2E test".into());
    }

    let dir = tempfile::tempdir()?;
    let home_dir = dir.path().join("home");
    let config_dir = dir.path().join("tui-config");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&home_dir)?;
    std::fs::create_dir_all(&config_dir)?;
    std::fs::create_dir_all(&agent_dir)?;

    let server = start_native_server(dir.path()).await?;
    let context = Context::background();
    let sessions = server.host.hold_as(
        Arc::new(lookup("p2.sessions")?),
        json!({}).into(),
        owner_caller(),
        &context,
    )?;
    sessions.wait(&context).await?;
    let created: Value = sessions
        .call(
            "create",
            json!({
                "cwd": dir.path(),
                "title": "Parity 1.5 E2E",
                "thinkingLevel": "off",
                "model": {"provider": "faux", "modelId": "faux-1"},
            })
            .into(),
            &context,
        )
        .await?
        .to_serde()?;
    assert!(created["id"].as_str().is_some());
    for _ in 0..50 {
        if let Ok(val) = sessions.read("sessions")
            && let Ok(v) = val.to_serde()
            && v.as_array().is_some_and(|a| !a.is_empty())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let origin = format!("http://{}", server.address);
    let p2_bin = env!("CARGO_BIN_EXE_p2");
    let cmd = format!("stty rows 24 cols 120 && exec '{p2_bin}' tui '{origin}' --theme dark");

    let mut child = Command::new("script")
        .arg("-qec")
        .arg(&cmd)
        .arg("/dev/null")
        .env("TERM", "xterm-256color")
        .env("HOME", &home_dir)
        .env("P2_DATA_DIR", dir.path())
        .env("P2_TUI_CONFIG_DIR", &config_dir)
        .env("PI_CODING_AGENT_DIR", &agent_dir)
        .env_remove("P2_TUI_OWNER_LINK")
        .env_remove("PORT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut stdin = child.stdin.take().ok_or("missing child stdin")?;
    let mut stdout = child.stdout.take().ok_or("missing child stdout")?;
    let captured = Arc::new(Mutex::new(Vec::new()));
    let cap_clone = captured.clone();
    let reader = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(mut guard) = cap_clone.lock() {
                        guard.extend_from_slice(&buf[..n]);
                    }
                }
            }
        }
    });

    let cfg_dir_for_thread = config_dir.clone();
    let test_result = tokio::task::spawn_blocking(move || -> TestResult {
        let mut offset =
            wait_for_screen_text(&captured, 0, "Parity 1.5 E2E", Duration::from_secs(15))?;

        // 1. F1 opens CommandPalette ("Commands"), Escape closes.
        send_raw(&mut stdin, b"\x1bOP")?;
        offset = wait_for_screen_text(&captured, offset, "Commands", Duration::from_secs(10))?;
        send_raw(&mut stdin, b"\x1b")?;
        thread::sleep(Duration::from_millis(150));

        // 2. /hotkeys opens scrollable TextOverlay (1-18 of ..., Launcher-owned), Enter closes.
        send_line(&mut stdin, "/hotkeys")?;
        offset = wait_for_screen_text(
            &captured,
            offset,
            "Keyboard shortcuts",
            Duration::from_secs(10),
        )?;
        offset = wait_for_screen_text(&captured, offset, "1-18 of", Duration::from_secs(5))?;
        send_raw(&mut stdin, b"\r")?;
        thread::sleep(Duration::from_millis(150));

        // 3. /settings exposes quiet-tools Tool calls & persists prefs.json; /keybindings opens Rebind keys.
        send_line(&mut stdin, "/settings")?;
        offset = wait_for_screen_text(&captured, offset, "Hide thinking", Duration::from_secs(10))?;
        send_raw(&mut stdin, b"\x1b[A")?; // wrap to last item ("Tool calls")
        offset = wait_for_screen_text(&captured, offset, "Tool calls", Duration::from_secs(10))?;
        send_raw(&mut stdin, b"\x1b[B")?; // wrap back to "Hide thinking"
        thread::sleep(Duration::from_millis(80));
        send_raw(&mut stdin, b"\r")?; // toggle Hide thinking
        thread::sleep(Duration::from_millis(150));
        send_raw(&mut stdin, b"\x1b")?;
        thread::sleep(Duration::from_millis(150));
        let prefs_text = std::fs::read_to_string(cfg_dir_for_thread.join("prefs.json"))?;
        assert!(
            prefs_text.contains("hideThinkingBlock"),
            "expected /settings edit to persist to prefs.json: {prefs_text}"
        );

        send_line(&mut stdin, "/keybindings")?;
        offset = wait_for_screen_text(&captured, offset, "Rebind keys", Duration::from_secs(10))?;
        send_raw(&mut stdin, b"\x1b")?;
        thread::sleep(Duration::from_millis(150));

        // 4. Ctrl+O toggles tool output expand, Escape runs cleanly, /reload works.
        send_raw(&mut stdin, b"\x0f")?;
        offset = wait_for_screen_text(
            &captured,
            offset,
            "Tool output expanded",
            Duration::from_secs(10),
        )?;
        send_raw(&mut stdin, b"\x0f")?;
        offset = wait_for_screen_text(
            &captured,
            offset,
            "Tool output collapsed",
            Duration::from_secs(10),
        )?;
        send_line(&mut stdin, "/reload")?;
        offset = wait_for_screen_text(&captured, offset, "TUI plugins", Duration::from_secs(10))?;

        // 5. Escape stop during a live chat turn.
        send_line(&mut stdin, "escape parity turn")?;
        thread::sleep(Duration::from_millis(100));
        send_raw(&mut stdin, b"\x1b")?;
        offset = wait_for_screen_text(&captured, offset, "Stopping", Duration::from_secs(10))?;

        // 6. Exercise interactive plugin overlays & panels.
        send_line(&mut stdin, "/resume")?;
        offset = wait_for_screen_text(
            &captured,
            offset,
            "Sessions (Enter switch",
            Duration::from_secs(10),
        )?;
        send_raw(&mut stdin, b"\x1b")?;
        thread::sleep(Duration::from_millis(150));

        for (cmd, expected) in [
            ("/envs", "Environments"),
            ("/ports", "Forwarded Ports"),
            ("/mcp", "MCP Servers"),
            ("/share", "Sharing"),
            ("/devices", "Paired Devices"),
            ("/artifacts", "Artifacts"),
            ("/chat-mode", "Chat Mode"),
            ("/logs", "Logs"),
            ("/notices", "Notices"),
            ("/downloads", "Downloads"),
            ("/agents", "Subagents"),
            ("/files", "Workspace Files"),
            ("/changes", "Workspace Changes"),
            ("/discord", "Discord"),
            ("/login", "Provider Login"),
            ("/prompts", "Prompt Templates"),
            ("/session", "Session:"),
        ] {
            send_line(&mut stdin, cmd)?;
            offset = wait_for_screen_text(&captured, offset, expected, Duration::from_secs(10))?;
        }
        send_line(&mut stdin, "/close-panel")?;
        thread::sleep(Duration::from_millis(150));

        // 7. `/quit` exits cleanly with status 0.
        send_line(&mut stdin, "/quit")?;
        drop(stdin);

        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err("p2 tui did not exit on /quit within timeout".into());
            }
            thread::sleep(Duration::from_millis(25));
        };
        let _ = reader.join();
        if !status.success() {
            return Err(format!("p2 tui exited with non-zero status on /quit: {status}").into());
        }
        Ok(())
    })
    .await?;

    sessions.release();
    server.close().await?;
    test_result
}
