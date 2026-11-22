//! The fixture programs of `packages/mcp/test/fixtures/*.mjs`, as modes of one small binary the
//! tests spawn over stdio, so the test suite needs no Node.

use std::io::{BufRead, Write};

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    match mode.as_str() {
        "stdio-server" => stdio_server(),
        "stubborn-server" => stubborn_server(),
        "exiting-parent-server" => exiting_parent_server(),
        "helper-sleep" => helper_sleep(),
        other => {
            eprintln!("unknown fixture mode: {other}");
        }
    }
}

/// `stdio-server.mjs`: a newline-delimited MCP server with one echo tool.
fn stdio_server() {
    let _ = writeln!(std::io::stderr(), "stdio fixture ready");
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(id) = message.get("id") else {
            continue;
        };
        let Some(method) = message.get("method").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let result = match method {
            "initialize" => serde_json::json!({
                "protocolVersion": "2025-06-18",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "stdio-fixture", "version": "1.0.0" },
            }),
            "tools/list" => serde_json::json!({
                "tools": [{ "name": "echo", "inputSchema": { "type": "object" } }],
            }),
            "tools/call" => serde_json::json!({
                "content": [{
                    "type": "text",
                    "text": message
                        .pointer("/params/arguments/text")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default(),
                }],
            }),
            "ping" => serde_json::json!({}),
            other => {
                let error = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": format!("not found: {other}") },
                });
                let _ = writeln!(std::io::stdout(), "{error}");
                continue;
            }
        };
        let response = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
        let _ = writeln!(std::io::stdout(), "{response}");
    }
}

/// `stubborn-server.mjs`: answers `initialize`, spawns a grandchild that outlives stdin, and
/// ignores stdin EOF and SIGTERM.
fn stubborn_server() {
    #[cfg(unix)]
    ignore_sigterm();
    let grandchild = std::process::Command::new(std::env::current_exe().unwrap_or_default())
        .arg("helper-sleep")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Ok(grandchild) = grandchild {
        let _ = writeln!(std::io::stderr(), "grandchild {}", grandchild.id());
    }
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(id) = message.get("id") else {
            continue;
        };
        if message.get("method").and_then(serde_json::Value::as_str) != Some("initialize") {
            continue;
        }
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "serverInfo": { "name": "stubborn-fixture", "version": "1.0.0" },
            },
        });
        let _ = writeln!(std::io::stdout(), "{response}");
    }
    // Ignores stdin closing: stays alive until killed.
    helper_sleep();
}

/// `exiting-parent-server.mjs`: starts a helper that inherits stdout and stderr, then exits while
/// the helper keeps running and holding the pipes.
fn exiting_parent_server() {
    let helper = std::process::Command::new(std::env::current_exe().unwrap_or_default())
        .arg("helper-sleep")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn();
    if let Ok(helper) = helper {
        let _ = writeln!(std::io::stderr(), "helper {}", helper.id());
    }
}

/// The grandchild and helper of the other fixtures: sleeps forever.
fn helper_sleep() {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

/// Ignores SIGTERM, like the stubborn fixture's signal handler.
#[cfg(unix)]
fn ignore_sigterm() {
    use nix::sys::signal::{SigAction, SigHandler, Signal};
    let ignore = SigAction::new(
        SigHandler::SigIgn,
        nix::sys::signal::SaFlags::empty(),
        nix::sys::signal::SigSet::empty(),
    );
    // SAFETY: the fixture is single-threaded here and installs a plain ignore action for SIGTERM,
    // capturing no state, so installing it cannot corrupt anything.
    let _ = unsafe { nix::sys::signal::sigaction(Signal::SIGTERM, &ignore) };
}
