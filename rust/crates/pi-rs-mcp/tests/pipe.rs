//! `packages/mcp/test/pipe.test.ts`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{oneshot, watch};

use pi_rs_mcp::{
    JsonRpcMessage, McpClient, McpClientOptions, McpPipe, McpPipeHandlers, McpPipeOpener,
    McpRequestOptions, McpTransport, McpTransportSendOptions, PipeData, PipeExited, PipeTransport,
    PipeTransportOptions,
};

#[path = "support/mod.rs"]
#[expect(
    dead_code,
    reason = "shared test helpers: each integration-test binary uses a different subset"
)]
mod support;

type OnStderr = Arc<dyn Fn(&str) + Send + Sync>;
type OpenedSender = Arc<Mutex<Option<oneshot::Sender<Arc<dyn McpPipe>>>>>;

use support::fixture_exe;

fn ping(id: i64) -> JsonRpcMessage {
    parse(&json!({ "jsonrpc": "2.0", "id": id, "method": "ping" }))
}

fn parse(value: &Value) -> JsonRpcMessage {
    pi_rs_mcp::parse_json_rpc_message(value).unwrap_or(JsonRpcMessage::Notification(
        pi_rs_mcp::JsonRpcNotification {
            jsonrpc: pi_rs_mcp::JsonRpcVersion,
            method: String::new(),
            params: None,
            extra: Default::default(),
        },
    ))
}

/// What the fake program of a test saw and how it ended.
#[derive(Clone)]
struct FakeProgram {
    transport: Arc<PipeTransport>,
    /// Calls of `close_stdin` and `close`, in order.
    calls: Arc<Mutex<Vec<&'static str>>>,
    written: Arc<Mutex<Vec<String>>>,
    messages: Arc<Mutex<Vec<Value>>>,
    errors: Arc<Mutex<Vec<String>>>,
    closed: Arc<std::sync::atomic::AtomicBool>,
    /// The handlers the transport connected to the program's output.
    handlers: Arc<Mutex<Option<McpPipeHandlers>>>,
    /// How the program's `exited` promise settles.
    exit_tx: watch::Sender<ExitState>,
    /// Held so the channel stays open; a watch channel closes without a receiver.
    _exit_rx: watch::Receiver<ExitState>,
}

/// A `PipeTransport` over an in-memory program that exits only when told to, or when killed.
async fn start_fake(options: PipeTransportOptionsWithoutOpen) -> FakeProgram {
    let (exited_tx, exited_rx) = watch::channel(ExitState::Running);
    let calls: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let written: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let handlers: Arc<Mutex<Option<McpPipeHandlers>>> = Arc::new(Mutex::new(None));
    let calls_for_pipe = calls.clone();
    let written_for_pipe = written.clone();
    let pipe = Arc::new(FakePipe {
        calls: calls_for_pipe,
        written: written_for_pipe,
        exited_tx: exited_tx.clone(),
    });
    let transport = PipeTransport::new(PipeTransportOptions {
        open: {
            let pipe = pipe.clone();
            let handlers = handlers.clone();
            let options = options.clone();
            Arc::new(move |open_handlers| {
                let pipe = pipe.clone();
                let handlers_slot = handlers.clone();
                let max_message_bytes = options.max_message_bytes;
                let max_stderr_bytes = options.max_stderr_bytes;
                let on_stderr = options.on_stderr.clone();
                let opened: futures_util::future::BoxFuture<
                    'static,
                    Result<Arc<dyn McpPipe>, String>,
                > = Box::pin(async move {
                    *handlers_slot.lock().unwrap() = Some(open_handlers);
                    let _ = (max_message_bytes, max_stderr_bytes, on_stderr);
                    Ok(pipe as Arc<dyn McpPipe>)
                });
                opened
            })
        } as McpPipeOpener,
        max_message_bytes: options.max_message_bytes,
        max_stderr_bytes: options.max_stderr_bytes,
        on_stderr: options.on_stderr,
        close_timeout_ms: options.close_timeout_ms,
    });
    let messages: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    transport.on_message(Arc::new({
        let messages = messages.clone();
        move |message: &JsonRpcMessage| {
            messages.lock().unwrap().push(message.to_json());
        }
    }));
    transport.on_error(Arc::new({
        let errors = errors.clone();
        move |error: &pi_rs_mcp::McpErrorEvent| {
            errors.lock().unwrap().push(error.message.clone());
        }
    }));
    transport.on_close(Arc::new({
        let closed = closed.clone();
        move || {
            closed.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }));
    transport.start().await.expect("start the fake program");
    FakeProgram {
        transport,
        calls,
        written,
        messages,
        errors,
        closed,
        handlers,
        exit_tx: exited_tx,
        _exit_rx: exited_rx,
    }
}

impl FakeProgram {
    fn stdout(&self, text: &str) {
        if let Some(handlers) = self.handlers.lock().unwrap().as_ref() {
            (handlers.on_stdout)(text.as_bytes());
        }
    }

    fn stderr(&self, text: &str) {
        if let Some(handlers) = self.handlers.lock().unwrap().as_ref() {
            (handlers.on_stderr)(text.as_bytes());
        }
    }

    fn exit(&self) {
        let _ = self.exit_tx.send(ExitState::Done);
    }

    fn fail(&self, message: &str) {
        let _ = self.exit_tx.send(ExitState::Failed(message.to_string()));
    }
}

#[derive(Clone, Default, PartialEq, Debug)]
enum ExitState {
    #[default]
    Running,
    Done,
    Failed(String),
}

struct FakePipe {
    calls: Arc<Mutex<Vec<&'static str>>>,
    written: Arc<Mutex<Vec<String>>>,
    exited_tx: watch::Sender<ExitState>,
}

impl McpPipe for FakePipe {
    fn write(&self, data: PipeData) {
        self.written
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(data.as_bytes()).into_owned());
    }

    fn close_stdin(&self) {
        self.calls.lock().unwrap().push("closeStdin");
    }

    fn close(&self) {
        self.calls.lock().unwrap().push("close");
        let _ = self
            .exited_tx
            .send(ExitState::Failed("aborted".to_string()));
    }

    fn exited(&self) -> PipeExited {
        let mut receiver = self.exited_tx.subscribe();
        Box::pin(async move {
            loop {
                match receiver.borrow_and_update().clone() {
                    ExitState::Running => {}
                    ExitState::Done => return Ok(()),
                    ExitState::Failed(message) => return Err(message),
                }
                if receiver.changed().await.is_err() {
                    return Ok(());
                }
            }
        })
    }
}

#[derive(Clone, Default)]
struct PipeTransportOptionsWithoutOpen {
    max_message_bytes: Option<u64>,
    max_stderr_bytes: Option<u64>,
    on_stderr: Option<OnStderr>,
    close_timeout_ms: Option<u64>,
}

/// Let the transport's reactions to `exited` run.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(10)).await;
}

#[tokio::test]
async fn joins_messages_split_across_chunks_and_splits_several_messages_in_one_chunk() {
    let program = start_fake(PipeTransportOptionsWithoutOpen::default()).await;
    let first = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }).to_string();
    let second = json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }).to_string();
    let third = json!({ "jsonrpc": "2.0", "id": 3, "method": "ping" }).to_string();
    program.stdout(&first[..10]);
    program.stdout(&format!("{}\n{}", &first[10..], second));
    program.stdout(&format!("\n{}\n", third));
    let messages = program.messages.lock().unwrap().clone();
    assert_eq!(
        messages,
        vec![
            json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }),
            json!({ "jsonrpc": "2.0", "id": 3, "method": "ping" }),
        ]
    );
    assert!(program.errors.lock().unwrap().is_empty());
}

#[tokio::test]
async fn strips_a_trailing_carriage_return_and_skips_blank_lines() {
    let program = start_fake(PipeTransportOptionsWithoutOpen::default()).await;
    let ping = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }).to_string();
    program.stdout(&format!("\n\r\n   \n{}\r\n\n", ping));
    let messages = program.messages.lock().unwrap().clone();
    assert_eq!(
        messages,
        vec![json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" })]
    );
    assert!(program.errors.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reports_invalid_messages_and_keeps_reading() {
    let program = start_fake(PipeTransportOptionsWithoutOpen::default()).await;
    let ping = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }).to_string();
    program.stdout(&format!("not json\n{{\"jsonrpc\":\"1.0\"}}\n{}\n", ping));
    let messages = program.messages.lock().unwrap().clone();
    assert_eq!(
        messages,
        vec![json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" })]
    );
    assert_eq!(program.errors.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn drops_messages_over_max_message_bytes_complete_or_not() {
    let options = PipeTransportOptionsWithoutOpen {
        max_message_bytes: Some(48),
        ..Default::default()
    };
    let program = start_fake(options).await;
    let oversized = json!({ "jsonrpc": "2.0", "method": "x".repeat(48) }).to_string();
    program.stdout(&format!(
        "{}\n{}\n",
        oversized,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" })
    ));
    program.stdout(&"y".repeat(49));
    program.stdout(&format!(
        "\n{}\n",
        json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" })
    ));
    let messages = program.messages.lock().unwrap().clone();
    assert_eq!(
        messages,
        vec![
            json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }),
        ]
    );
    let errors = program.errors.lock().unwrap().clone();
    assert_eq!(
        errors,
        vec![
            "MCP stdio message exceeds 48 bytes".to_string(),
            "MCP stdio message exceeds 48 bytes".to_string(),
        ]
    );
}

#[tokio::test]
async fn keeps_a_bounded_stderr_tail_and_passes_every_chunk_to_on_stderr() {
    let chunks: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let options = PipeTransportOptionsWithoutOpen {
        max_stderr_bytes: Some(8),
        on_stderr: Some(Arc::new({
            let chunks = chunks.clone();
            move |chunk: &str| {
                chunks.lock().unwrap().push(chunk.to_string());
            }
        })),
        ..Default::default()
    };
    let program = start_fake(options).await;
    program.stderr("starting\n");
    program.stderr("ready\n");
    let chunks = chunks.lock().unwrap().clone();
    assert_eq!(
        chunks,
        vec!["starting\n".to_string(), "ready\n".to_string()]
    );
    assert_eq!(program.transport.stderr(), "g\nready\n");
}

#[tokio::test]
async fn writes_each_message_as_one_line() {
    let program = start_fake(PipeTransportOptionsWithoutOpen::default()).await;
    program
        .transport
        .send(&ping(1), &McpTransportSendOptions::default())
        .await
        .expect("send");
    let written = program.written.lock().unwrap().clone();
    assert_eq!(
        written,
        vec![format!(
            "{}\n",
            json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" })
        )]
    );
}

#[tokio::test]
async fn reports_an_incomplete_message_when_the_program_exits_then_closes() {
    let program = start_fake(PipeTransportOptionsWithoutOpen::default()).await;
    program.stdout("{\"jsonrpc\":\"2.0\"");
    program.exit();
    settle().await;
    let errors = program.errors.lock().unwrap().clone();
    assert_eq!(
        errors,
        vec!["MCP stdio server closed with an incomplete JSON-RPC message".to_string()]
    );
    assert!(program.closed.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn reports_a_failed_exit_it_did_not_cause_then_closes() {
    let program = start_fake(PipeTransportOptionsWithoutOpen::default()).await;
    program.fail("connection lost");
    settle().await;
    let errors = program.errors.lock().unwrap().clone();
    assert_eq!(errors, vec!["connection lost".to_string()]);
    assert!(program.closed.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn rejects_send_after_close_and_after_the_program_ended() {
    let closed_by_us = start_fake(PipeTransportOptionsWithoutOpen::default()).await;
    closed_by_us.exit();
    closed_by_us.transport.close().await;
    let error = closed_by_us
        .transport
        .send(&ping(1), &McpTransportSendOptions::default())
        .await
        .expect_err("closed");
    assert!(matches!(
        error,
        pi_rs_mcp::McpTransportError::ConnectionClosed(_)
    ));

    let ended = start_fake(PipeTransportOptionsWithoutOpen::default()).await;
    ended.exit();
    settle().await;
    let error = ended
        .transport
        .send(&ping(1), &McpTransportSendOptions::default())
        .await
        .expect_err("ended");
    assert!(matches!(
        error,
        pi_rs_mcp::McpTransportError::ConnectionClosed(_)
    ));
    assert!(ended.written.lock().unwrap().is_empty());
}

#[tokio::test]
async fn closes_stdin_and_does_not_kill_a_program_that_exits_in_time() {
    let program = start_fake(PipeTransportOptionsWithoutOpen::default()).await;
    let closing = tokio::spawn({
        let transport = program.transport.clone();
        async move { transport.close().await }
    });
    settle().await;
    let calls = program.calls.lock().unwrap().clone();
    assert_eq!(calls, vec!["closeStdin"]);
    let error = program
        .transport
        .send(&ping(1), &McpTransportSendOptions::default())
        .await
        .expect_err("closed");
    assert!(matches!(
        error,
        pi_rs_mcp::McpTransportError::ConnectionClosed(_)
    ));
    assert!(program.written.lock().unwrap().is_empty());
    program.exit();
    closing.await.expect("close resolves");
    let calls = program.calls.lock().unwrap().clone();
    assert_eq!(calls, vec!["closeStdin"]);
    assert!(program.closed.load(std::sync::atomic::Ordering::SeqCst));
    assert!(program.errors.lock().unwrap().is_empty());
}

#[tokio::test]
async fn shuts_down_once_when_closed_twice() {
    let program = start_fake(PipeTransportOptionsWithoutOpen::default()).await;
    let first = tokio::spawn({
        let transport = program.transport.clone();
        async move { transport.close().await }
    });
    settle().await;
    let second = tokio::spawn({
        let transport = program.transport.clone();
        async move { transport.close().await }
    });
    program.exit();
    first.await.expect("first close");
    second.await.expect("second close");
    program.transport.close().await;
    let calls = program.calls.lock().unwrap().clone();
    assert_eq!(calls, vec!["closeStdin"]);
    assert!(program.closed.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn kills_a_program_only_after_close_timeout() {
    let options = PipeTransportOptionsWithoutOpen {
        close_timeout_ms: Some(50),
        ..Default::default()
    };
    let program = start_fake(options).await;
    let closing = tokio::spawn({
        let transport = program.transport.clone();
        async move { transport.close().await }
    });
    settle().await;
    // Before the timeout, only the stdin was closed.
    let calls = program.calls.lock().unwrap().clone();
    assert_eq!(calls, vec!["closeStdin"]);
    closing.await.expect("close resolves after killing");
    let calls = program.calls.lock().unwrap().clone();
    assert_eq!(calls, vec!["closeStdin", "close"]);
    assert!(program.closed.load(std::sync::atomic::Ordering::SeqCst));
    // The rejection of `exited` that killing caused is not an error.
    assert!(program.errors.lock().unwrap().is_empty());
}

#[tokio::test]
async fn rejects_start_when_open_rejects() {
    let transport = PipeTransport::new(PipeTransportOptions {
        open: Arc::new(|_handlers| Box::pin(async { Err("not_supported".to_string()) })),
        max_message_bytes: None,
        max_stderr_bytes: None,
        on_stderr: None,
        close_timeout_ms: None,
    });
    let error = transport.start().await.expect_err("open fails");
    assert!(error.to_string().contains("not_supported"));
}

#[tokio::test]
async fn kills_a_program_that_finished_starting_after_close() {
    // Like upstream's `Promise.withResolvers()`: the open holds until the test releases it, so
    // `close` runs while the program is starting.
    let (release_open_tx, release_open_rx) = oneshot::channel::<()>();
    let release_open: Arc<Mutex<Option<oneshot::Receiver<()>>>> =
        Arc::new(Mutex::new(Some(release_open_rx)));
    let (opened_tx, opened_rx) = oneshot::channel::<Arc<dyn McpPipe>>();
    let opened: OpenedSender = Arc::new(Mutex::new(Some(opened_tx)));
    let (exited_tx, _exited_rx) = watch::channel(ExitState::Running);
    let pipe_close_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let pipe_close_calls_for_open = pipe_close_calls.clone();
    let transport = PipeTransport::new(PipeTransportOptions {
        open: Arc::new(move |_handlers| {
            let release_open = release_open.clone();
            let opened = opened.clone();
            let exited_tx = exited_tx.clone();
            let pipe_close_calls = pipe_close_calls_for_open.clone();
            let opened_future: futures_util::future::BoxFuture<
                'static,
                Result<Arc<dyn McpPipe>, String>,
            > = Box::pin(async move {
                let release = release_open.lock().unwrap().take();
                if let Some(release) = release {
                    let _ = release.await;
                }
                let pipe: Arc<dyn McpPipe> = Arc::new(CountingClosePipe {
                    exited_tx,
                    close_calls: pipe_close_calls,
                });
                let sender = opened.lock().unwrap().take();
                if let Some(sender) = sender {
                    let _ = sender.send(pipe.clone());
                }
                Ok(pipe)
            });
            opened_future
        }),
        max_message_bytes: None,
        max_stderr_bytes: None,
        on_stderr: None,
        close_timeout_ms: Some(100),
    });
    // Upstream's `start()` runs as soon as it is called; the spawned task gives the Rust
    // future that eagerness, so the open is in flight before `close` runs.
    let starting = tokio::spawn({
        let transport = transport.clone();
        async move { transport.start().await }
    });
    settle().await;
    transport.close().await;
    let _ = release_open_tx.send(());
    let pipe = opened_rx.await.expect("open resolves after close");
    let _ = pipe.exited().await; // The rejection of `exited` that killing caused is observed.
    let error = starting
        .await
        .expect("join start")
        .expect_err("start after close");
    assert!(matches!(
        error,
        pi_rs_mcp::McpTransportError::ConnectionClosed(_)
    ));
    assert_eq!(
        pipe_close_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the transport killed the program it found running after close"
    );
}

struct CountingClosePipe {
    exited_tx: watch::Sender<ExitState>,
    close_calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl McpPipe for CountingClosePipe {
    fn write(&self, _data: PipeData) {}
    fn close_stdin(&self) {}
    fn close(&self) {
        self.close_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _ = self
            .exited_tx
            .send(ExitState::Failed("aborted".to_string()));
    }
    fn exited(&self) -> PipeExited {
        let mut receiver = self.exited_tx.subscribe();
        Box::pin(async move {
            loop {
                match receiver.borrow_and_update().clone() {
                    ExitState::Running => {}
                    ExitState::Done => return Ok(()),
                    ExitState::Failed(message) => return Err(message),
                }
                if receiver.changed().await.is_err() {
                    return Ok(());
                }
            }
        })
    }
}

/// Runs a program with a real process, standing in for pi-env's `openProcess`.
#[tokio::test]
async fn runs_initialize_tools_list_and_tools_call_with_a_real_program() {
    let transport = PipeTransport::new(PipeTransportOptions {
        open: Arc::new(|handlers| {
            Box::pin(async move {
                let mut child = tokio::process::Command::new(fixture_exe())
                    .arg("stdio-server")
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .map_err(|error| error.to_string())?;
                let mut stdout = child.stdout.take();
                let mut stderr = child.stderr.take();
                let stdin = child.stdin.take();
                tokio::spawn(async move {
                    let mut chunk = [0u8; 4096];
                    use tokio::io::AsyncReadExt;
                    while let Some(stdout) = stdout.as_mut() {
                        match stdout.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(read) => (handlers.on_stdout)(&chunk[..read]),
                        }
                    }
                });
                tokio::spawn(async move {
                    let mut chunk = [0u8; 4096];
                    use tokio::io::AsyncReadExt;
                    while let Some(stderr) = stderr.as_mut() {
                        match stderr.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(read) => (handlers.on_stderr)(&chunk[..read]),
                        }
                    }
                });
                Ok(RealProgramPipe::spawn(child, stdin) as Arc<dyn McpPipe>)
            })
        }),
        max_message_bytes: None,
        max_stderr_bytes: None,
        on_stderr: None,
        close_timeout_ms: None,
    });
    let client = McpClient::new(McpClientOptions {
        name: "pipe-test".to_string(),
        version: "1.0.0".to_string(),
        ..McpClientOptions::default()
    });
    let initialize = client.connect(transport.clone()).await.expect("connect");
    assert_eq!(initialize.server_info.name, "stdio-fixture");
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
    assert!(transport.stderr().contains("stdio fixture ready"));
    client.close().await;
    assert_eq!(client.connection_state(), pi_rs_mcp::ClientState::Closed);
}

struct RealProgramPipe {
    /// The child's id, to kill on `close`.
    pid: Option<u32>,
    /// Writes queue here; the writer task owns the stdin handle, like Node's stream buffer.
    writes: Mutex<Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>,
    /// Set once the child was reaped; every `exited()` await watches it.
    reaped: watch::Sender<bool>,
}

impl RealProgramPipe {
    /// Creates the pipe: one writer task owns the stdin, and the reaper watches the child, so
    /// `exited` settles however often it is awaited.
    fn spawn(child: tokio::process::Child, stdin: Option<tokio::process::ChildStdin>) -> Arc<Self> {
        let (writes, mut writes_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        if let Some(mut stdin) = stdin {
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                while let Some(bytes) = writes_rx.recv().await {
                    let _ = stdin.write_all(&bytes).await;
                }
                // The sender is gone: the stdin handle drops here, closing it.
            });
        }
        let pid = child.id();
        let mut reap_child = child;
        let (reaped, reaped_rx) = watch::channel(false);
        let reaped_tx = reaped.clone();
        tokio::spawn(async move {
            let _ = reap_child.wait().await;
            let _ = reaped_tx.send(true);
        });
        let _ = reaped_rx;
        Arc::new(Self {
            pid,
            writes: Mutex::new(Some(writes)),
            reaped,
        })
    }
}

impl McpPipe for RealProgramPipe {
    fn write(&self, data: PipeData) {
        if let Some(writes) = self.writes.lock().unwrap().as_ref() {
            let _ = writes.send(data.as_bytes().to_vec());
        }
    }

    fn close_stdin(&self) {
        // Dropping the sender ends the writer task, which drops the stdin handle.
        *self.writes.lock().unwrap() = None;
    }

    fn close(&self) {
        *self.writes.lock().unwrap() = None;
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            use nix::sys::signal::{Signal, kill};
            use nix::unistd::Pid;
            let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
        }
    }

    fn exited(&self) -> PipeExited {
        let mut reaped = self.reaped.subscribe();
        Box::pin(async move {
            while !*reaped.borrow_and_update() {
                if reaped.changed().await.is_err() {
                    return Ok(());
                }
            }
            Ok(())
        })
    }
}
