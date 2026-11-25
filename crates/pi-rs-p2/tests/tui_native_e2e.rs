//! Live native `P2Server` + real `p2 tui` binary PTY end-to-end test.

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_native_server_and_p2_tui_pty_e2e() -> TestResult {
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
                "title": "Native TUI E2E",
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
    let cmd = format!("stty rows 24 cols 120 && exec '{p2_bin}' tui '{origin}'");

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
    let reader_buf = captured.clone();
    let reader = thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    reader_buf
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .extend_from_slice(&chunk[..n]);
                }
            }
        }
    });

    let test_result: TestResult = tokio::task::spawn_blocking(move || {
        // 1. Local owner auto-sign-in succeeds and renders the session header.
        let mut offset =
            wait_for_screen_text(&captured, 0, "Native TUI E2E", Duration::from_secs(15))?;

        // 2. Sending a chat message produces an `Echo:` reply from real `p2 --worker`.
        let chat_start = offset;
        send_line(&mut stdin, "hello native tui")?;
        let echo_offset =
            wait_for_screen_text(&captured, chat_start, "Echo:", Duration::from_secs(15))?;
        offset = wait_for_screen_text(
            &captured,
            chat_start,
            "hello native tui",
            Duration::from_secs(10),
        )?
        .max(echo_offset);

        // 3. `/help`, `/models`, and `/tree` open their respective side panels, and `/close-panel` dismisses them.
        send_line(&mut stdin, "/help")?;
        offset = wait_for_screen_text(
            &captured,
            offset,
            "Built-in Commands",
            Duration::from_secs(10),
        )?;
        send_line(&mut stdin, "/close-panel")?;
        thread::sleep(Duration::from_millis(200));

        send_line(&mut stdin, "/models")?;
        offset = wait_for_screen_text(
            &captured,
            offset,
            "Configured Models",
            Duration::from_secs(10),
        )?;
        send_line(&mut stdin, "/close-panel")?;
        thread::sleep(Duration::from_millis(200));

        send_line(&mut stdin, "/tree")?;
        let tree_start = offset;
        offset = wait_for_screen_text(
            &captured,
            tree_start,
            "Conversation Tree",
            Duration::from_secs(10),
        )?;
        wait_for_screen_text(
            &captured,
            tree_start,
            "hello native tui",
            Duration::from_secs(10),
        )?;
        send_line(&mut stdin, "/close-panel")?;
        thread::sleep(Duration::from_millis(200));

        send_line(&mut stdin, "/cost")?;
        offset = wait_for_screen_text(&captured, offset, "Responses (1)", Duration::from_secs(10))?;
        send_line(&mut stdin, "/close-panel")?;
        thread::sleep(Duration::from_millis(200));

        send_line(&mut stdin, "/status")?;
        offset =
            wait_for_screen_text(&captured, offset, "Session Status", Duration::from_secs(10))?;
        send_line(&mut stdin, "/close-panel")?;
        thread::sleep(Duration::from_millis(200));

        send_line(&mut stdin, "/new Second Session")?;
        wait_for_screen_text(&captured, offset, "Second Session", Duration::from_secs(10))?;

        // 4. `Ctrl+C` exits `p2 tui` cleanly with status 0.
        stdin.write_all(b"\x03")?;
        stdin.flush()?;
        drop(stdin);

        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err("p2 tui did not exit on Ctrl+C within timeout".into());
            }
            thread::sleep(Duration::from_millis(25));
        };
        let _ = reader.join();
        if !status.success() {
            return Err(format!("p2 tui exited with non-zero status: {status}").into());
        }
        Ok(())
    })
    .await?;

    sessions.release();
    server.close().await?;
    test_result
}
