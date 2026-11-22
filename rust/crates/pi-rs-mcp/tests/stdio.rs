//! `packages/mcp/test/stdio.test.ts`.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use pi_rs_mcp::{
    McpClient, McpClientOptions, McpRequestOptions, McpTransport, StdioTransport,
    StdioTransportOptions,
};

#[path = "support/mod.rs"]
#[expect(
    dead_code,
    reason = "shared test helpers: each integration-test binary uses a different subset"
)]
mod support;

use support::{fixture_exe, wait_for};

fn is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        use nix::sys::signal::kill;
        use nix::unistd::Pid;
        // Signal 0 probes existence; a zombie still counts, as in Node.
        kill(Pid::from_raw(pid as i32), None).is_ok()
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn parse_pid(stderr: &str, marker: &str) -> Option<u32> {
    stderr
        .lines()
        .find_map(|line| line.strip_prefix(marker))
        .and_then(|digits| digits.trim().parse().ok())
}

#[tokio::test]
async fn connects_to_a_newline_delimited_mcp_server_and_captures_stderr() {
    let stderr: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let transport = StdioTransport::new(StdioTransportOptions {
        command: fixture_exe(),
        args: vec!["stdio-server".to_string()],
        on_stderr: Some(Arc::new({
            let stderr = stderr.clone();
            move |chunk: &str| {
                stderr
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(chunk.to_string());
            }
        })),
        ..StdioTransportOptions::default()
    });
    let client = McpClient::new(McpClientOptions {
        name: "stdio-test".to_string(),
        version: "1.0.0".to_string(),
        ..McpClientOptions::default()
    });
    let initialize = client.connect(transport.clone()).await.expect("connect");
    assert_eq!(
        initialize.server_info.name, "stdio-fixture",
        "the fixture answers initialize"
    );
    let tools = client
        .list_tools(&McpRequestOptions::default())
        .await
        .expect("listTools");
    assert_eq!(
        tools
            .iter()
            .map(|tool| serde_json::to_value(tool).unwrap_or_default())
            .collect::<Vec<_>>(),
        vec![json!({ "name": "echo", "inputSchema": { "type": "object" } })]
    );
    let result = client
        .call_tool(
            "echo",
            Some(json!({ "text": "hello" })),
            &McpRequestOptions::default(),
        )
        .await
        .expect("callTool");
    assert_eq!(
        serde_json::to_value(&result).unwrap_or_default(),
        json!({ "content": [{ "type": "text", "text": "hello" }] })
    );
    assert!(transport.pid().is_some());
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(
        stderr
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .join("")
            .contains("stdio fixture ready")
    );
    assert!(transport.stderr().contains("stdio fixture ready"));
    client.close().await;
    assert_eq!(client.connection_state(), pi_rs_mcp::ClientState::Closed);
}

#[cfg(unix)]
#[tokio::test]
async fn kills_a_server_that_ignores_shutdown_including_its_children() {
    let transport = StdioTransport::new(StdioTransportOptions {
        command: fixture_exe(),
        args: vec!["stubborn-server".to_string()],
        close_timeout_ms: Some(100),
        ..StdioTransportOptions::default()
    });
    let client = McpClient::new(McpClientOptions {
        name: "stdio-test".to_string(),
        version: "1.0.0".to_string(),
        ..McpClientOptions::default()
    });
    client.connect(transport.clone()).await.expect("connect");
    let mut grandchild: Option<u32> = None;
    for _ in 0..200 {
        if let Some(pid) = parse_pid(&transport.stderr(), "grandchild ") {
            grandchild = Some(pid);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let grandchild = grandchild.expect("the fixture reports its grandchild");

    let started = std::time::Instant::now();
    client.close().await;
    assert!(started.elapsed() < Duration::from_secs(5));
    let mut alive = true;
    for _ in 0..200 {
        if !is_alive(grandchild) {
            alive = false;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !alive,
        "the grandchild died with the server's process group"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn kills_children_still_holding_the_pipes_after_the_server_process_exited() {
    let transport = StdioTransport::new(StdioTransportOptions {
        command: fixture_exe(),
        args: vec!["exiting-parent-server".to_string()],
        close_timeout_ms: Some(100),
        ..StdioTransportOptions::default()
    });
    let close_events = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    transport.on_close(Arc::new({
        let close_events = close_events.clone();
        move || {
            close_events.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }));
    transport.start().await.expect("start");
    let server_pid = transport.pid().expect("server pid");
    let mut helper: Option<u32> = None;
    for _ in 0..200 {
        if let Some(pid) = parse_pid(&transport.stderr(), "helper ") {
            helper = Some(pid);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let helper = helper.expect("the fixture reports its helper");
    assert!(
        wait_for(|| !is_alive(server_pid), 2_000).await,
        "the server exited"
    );
    assert!(is_alive(helper), "the helper holds the pipes");
    assert_eq!(
        close_events.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the transport is still open while the pipes are held"
    );

    transport.close().await;
    assert!(
        wait_for(|| !is_alive(helper), 2_000).await,
        "closing killed the helper"
    );
    assert_eq!(close_events.load(std::sync::atomic::Ordering::SeqCst), 1);
}
