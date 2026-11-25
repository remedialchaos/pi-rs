use std::{
    io,
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn pi(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
    command
        .current_dir(root)
        .env_clear()
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("PI_CODING_AGENT_DIR", root.join("agent"))
        .env("PI_OFFLINE", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null());
    command
}

#[test]
fn metadata_and_argument_errors_use_their_own_streams_and_status() -> TestResult {
    let root = tempfile::tempdir()?;
    for flag in ["--version", "-v"] {
        let output = pi(root.path()).arg(flag).output()?;
        assert!(output.status.success());
        assert_eq!(output.stdout, b"1.6.0\n");
        assert!(output.stderr.is_empty());
    }
    for flag in ["--help", "-h"] {
        let output = pi(root.path()).arg(flag).output()?;
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        let help = String::from_utf8(output.stdout)?;
        assert!(help.contains("Usage:\n  pi [options] [--] [@files...] [messages...]\n"));
        assert!(help.contains("--mode <mode>"));
    }
    for (args, error) in [
        (
            &["--mode", "wrong"][..],
            "Error: Invalid mode \"wrong\". Valid values: text, json, rpc\n",
        ),
        (
            &["--nonesuch", "argument"][..],
            "Error: Unknown option: --nonesuch\n",
        ),
        (
            &["--mode", "rpc", "@a.txt"][..],
            "Error: @file arguments are not supported in RPC mode\n",
        ),
        (
            &["--fork", "old", "--no-session"][..],
            "Error: --fork cannot be combined with --no-session\n",
        ),
    ] {
        let output = pi(root.path()).args(args).output()?;
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert_eq!(String::from_utf8(output.stderr)?, error, "{args:?}");
    }
    Ok(())
}

#[test]
fn list_models_filters_auth_and_applies_the_search_pattern() -> TestResult {
    let root = tempfile::tempdir()?;
    let output = pi(root.path())
        .args(["--list-models", "gpt-4o-mini"])
        .env("OPENAI_API_KEY", "offline-fixture")
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout)?;
    let mut rows = stdout.lines();
    assert!(rows.next().is_some_and(|line| line.starts_with("provider")));
    assert!(
        rows.next()
            .is_some_and(|line| line.starts_with("openai") && line.contains("gpt-4o-mini"))
    );
    for line in rows {
        assert!(line.starts_with("openai") && line.contains("gpt-4o-mini"));
    }
    Ok(())
}

#[test]
fn export_resolves_session_path_and_writes_the_requested_file() -> TestResult {
    let root = tempfile::tempdir()?;
    let header = json!({ "type": "session", "version": 3, "id": "sample", "timestamp": "2026-01-01T00:00:00.000Z", "cwd": root.path() });
    let message = json!({ "type": "message", "id": "1", "parentId": null, "timestamp": "2026-01-01T00:00:00.000Z",
        "message": { "role": "user", "content": "Exported <user> message", "timestamp": 1767225600000_u64 } });
    std::fs::write(
        root.path().join("session.jsonl"),
        format!("{header}\n{message}\n"),
    )?;
    let output = pi(root.path())
        .args(["--export", "session.jsonl", "conversation.html"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"Exported to: conversation.html\n");
    let html = std::fs::read_to_string(root.path().join("conversation.html"))?;
    assert!(html.contains("Exported &lt;user&gt; message"));
    let missing = pi(root.path())
        .args(["--export", "missing.jsonl"])
        .output()?;
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(
        std::str::from_utf8(&missing.stderr)?,
        format!(
            "Error: File not found: {}\n",
            root.path().join("missing.jsonl").display()
        )
    );
    Ok(())
}

#[tokio::test]
async fn json_emits_system_and_user_turns_through_a_configured_provider() -> TestResult {
    let root = tempfile::tempdir()?;
    let agent = root.path().join("agent");
    std::fs::create_dir(&agent)?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let overlay = json!({"providers": {"local": {
        "baseUrl": format!("http://{}/v1", listener.local_addr()?),
        "api": "openai-completions",
        "apiKey": "fixture-only",
        "models": [{"id": "wire-test", "contextWindow": 32000, "maxTokens": 1024}]
    }}});
    std::fs::write(agent.join("models.json"), serde_json::to_vec(&overlay)?)?;
    let mut child = tokio::process::Command::from(pi(root.path()))
        .args([
            "--mode",
            "json",
            "--no-session",
            "--no-tools",
            "--system-prompt",
            "fixture instructions",
            "--provider",
            "local",
            "--model",
            "wire-test",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("missing child stdin")?
        .write_all(b"piped prompt\n")
        .await?;
    let serve = async {
        let (socket, _) = listener.accept().await?;
        let mut reader = BufReader::new(socket);
        let mut head = String::new();
        let mut length = None;
        let mut authorized = false;
        reader.read_line(&mut head).await?;
        assert_eq!(head, "POST /v1/chat/completions HTTP/1.1\r\n");
        loop {
            head.clear();
            if reader.read_line(&mut head).await? == 0 || head == "\r\n" {
                break;
            }
            if let Some((name, value)) = head.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    length = Some(value.trim().parse::<usize>()?);
                } else if name.eq_ignore_ascii_case("authorization") {
                    authorized = value.trim() == "Bearer fixture-only";
                }
            }
        }
        assert!(authorized);
        let mut body = vec![0; length.ok_or("missing request length")?];
        reader.read_exact(&mut body).await?;
        let body: Value = serde_json::from_slice(&body)?;
        let last_user = body["messages"]
            .as_array()
            .and_then(|messages| {
                messages
                    .iter()
                    .rev()
                    .find(|message| message["role"] == "user")
            })
            .ok_or("missing user turn")?;
        let content = &last_user["content"];
        assert!(
            content == "piped prompt"
                || content == &json!([{"type": "text", "text": "piped prompt"}]),
            "{content}"
        );
        let data = concat!(
            "data: {\"id\":\"reply\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"local answer\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        reader.get_mut().write_all(format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}", data.len()
        ).as_bytes()).await?;
        Ok::<_, Box<dyn std::error::Error>>(())
    };
    let observe = async {
        let output = child.wait_with_output().await?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let frames: Vec<Value> = std::str::from_utf8(&output.stdout)?
            .split_terminator('\n')
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?;
        let messages: Vec<_> = frames
            .iter()
            .filter(|frame| frame["type"] == "message_end")
            .map(|frame| &frame["message"])
            .collect();
        let [system, user, assistant] = messages.as_slice() else {
            return Err("missing system, user, or assistant message".into());
        };
        assert_eq!(system["role"], "system");
        assert_eq!(user["role"], "user");
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(system["sections"]["preamble"], "fixture instructions");
        assert_eq!(assistant["content"][0]["text"], "local answer");
        assert!(output.stderr.is_empty());
        Ok::<_, Box<dyn std::error::Error>>(())
    };
    tokio::time::timeout(Duration::from_secs(30), async {
        tokio::try_join!(serve, observe)
    })
    .await??;
    Ok(())
}

async fn response(
    input: &mut (impl AsyncBufRead + Unpin),
    id: &str,
) -> Result<Value, Box<dyn std::error::Error>> {
    tokio::time::timeout(Duration::from_secs(8), async {
        let mut line = String::new();
        loop {
            line.clear();
            if input.read_line(&mut line).await? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("no response for {id}"),
                )
                .into());
            }
            let value: Value = serde_json::from_str(&line)?;
            if value["type"] == "response" && value["id"] == id {
                return Ok(value);
            }
        }
    })
    .await?
}

#[tokio::test]
async fn live_rpc_replies_without_eof_and_keeps_processing_after_errors() -> TestResult {
    let root = tempfile::tempdir()?;
    std::fs::create_dir_all(root.path().join("agent/prompts"))?;
    std::fs::write(
        root.path().join("agent/prompts/inspect.md"),
        "---\ndescription: Inspect local state\n---\nInspect $1",
    )?;
    let mut command = pi(root.path());
    command
        .args([
            "--mode",
            "rpc",
            "--no-session",
            "--provider",
            "openai",
            "--model",
            "gpt-4o",
            "--api-key",
            "offline-fixture",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = tokio::process::Command::from(command)
        .kill_on_drop(true)
        .spawn()?;
    let mut input = child.stdin.take().ok_or("missing child stdin")?;
    let mut output = tokio::io::BufReader::new(child.stdout.take().ok_or("missing child stdout")?);
    input
        .write_all(b"{\"id\":\"state\",\"type\":\"get_state\"}\r\n")
        .await?;
    let state = response(&mut output, "state").await?;
    assert_eq!(state["data"]["model"]["id"], "gpt-4o");
    assert_eq!(state["data"]["messageCount"], 0);
    assert_eq!(state["data"]["steeringMode"], "one-at-a-time");
    assert!(state["data"].get("sessionFile").is_none());
    input
        .write_all(b"{\"id\":\"commands\",\"type\":\"get_commands\"}\n")
        .await?;
    let commands = response(&mut output, "commands").await?;
    let commands = commands["data"]["commands"]
        .as_array()
        .ok_or("missing command list")?;
    assert!(
        commands
            .iter()
            .any(|command| command["name"] == "inspect" && command["source"] == "prompt")
    );
    input
        .write_all(
            "{\"id\":\"name\",\"type\":\"set_session_name\",\"name\":\" one\u{2028}two \"}\n"
                .as_bytes(),
        )
        .await?;
    assert_eq!(response(&mut output, "name").await?["success"], true);
    input.write_all(b"{\"id\":\"bad\",\"type\":\"does_not_exist\"}\n{\"id\":\"again\",\"type\":\"get_state\"}\n").await?;
    assert_eq!(
        response(&mut output, "bad").await?,
        json!({"id":"bad","type":"response","command":"does_not_exist","success":false,"error":"Unknown command: does_not_exist"})
    );
    let renamed = response(&mut output, "again").await?;
    assert_eq!(renamed["data"]["sessionName"], "one\u{2028}two");
    assert_eq!(renamed["data"]["sessionId"], state["data"]["sessionId"]);
    drop(input);
    let final_output =
        tokio::time::timeout(Duration::from_secs(8), child.wait_with_output()).await??;
    assert!(
        final_output.status.success(),
        "{}",
        String::from_utf8_lossy(&final_output.stderr)
    );
    Ok(())
}
