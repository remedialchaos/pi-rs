use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    io,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use pi_rs_agent::{AgentMessage, QueueMode};
use pi_rs_ai::{ImageContent, Model, ThinkingLevel};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::{Child, Command},
    sync::{broadcast, mpsc, oneshot},
    task::JoinHandle,
    time::{sleep, timeout},
};

use super::{
    JsonlReader, RpcBashResult, RpcCancelled, RpcCommand, RpcCompactionResult, RpcDisposition,
    RpcEntries, RpcExtensionUiResponse, RpcForkMessage, RpcForkResult, RpcModelCycle, RpcQueue,
    RpcRequest, RpcResponse, RpcSessionState, RpcSessionStats, RpcSlashCommand,
    RpcStreamingBehavior, RpcTree, serialize_json_line,
};

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Default)]
pub struct RpcClientOptions {
    /// Defaults to `pi` on PATH.
    pub cli_path: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<OsString, OsString>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub args: Vec<OsString>,
}

/// A multiplexed RPC connection. Clone neither a connection nor a stream; concurrent calls may
/// share `&RpcClient`, and each call is correlated by its own request id.
pub struct RpcClient {
    options: RpcClientOptions,
    child: Option<Child>,
    transport: Option<Transport>,
    stderr: Arc<Mutex<Vec<u8>>>,
    stderr_task: Option<JoinHandle<()>>,
    counter: AtomicU64,
}

struct Transport {
    outbound: mpsc::UnboundedSender<Vec<u8>>,
    shared: Arc<Shared>,
    task: JoinHandle<()>,
}

struct Shared {
    events: broadcast::Sender<Value>,
    requests: Mutex<Requests>,
    closed: tokio::sync::Notify,
}

#[derive(Default)]
struct Requests {
    pending: HashMap<String, oneshot::Sender<Result<RpcResponse, Arc<str>>>>,
    exit_error: Option<Arc<str>>,
}

impl Shared {
    fn fail(&self, error: String) {
        let mut requests = self
            .requests
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if requests.exit_error.is_some() {
            return;
        }
        let error: Arc<str> = error.into();
        requests.exit_error = Some(error.clone());
        for (_, pending) in requests.pending.drain() {
            let _ = pending.send(Err(error.clone()));
        }
        self.closed.notify_waiters();
    }
}

struct Pending<'a> {
    shared: &'a Shared,
    id: String,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        self.shared
            .requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending
            .remove(&self.id);
    }
}

impl Transport {
    fn new(
        read: impl AsyncRead + Unpin + Send + 'static,
        mut write: impl AsyncWrite + Unpin + Send + 'static,
    ) -> Self {
        let (outbound, mut frames) = mpsc::unbounded_channel::<Vec<u8>>();
        let (events, _) = broadcast::channel(1024);
        let shared = Arc::new(Shared {
            events,
            requests: Mutex::default(),
            closed: tokio::sync::Notify::new(),
        });
        let connection = shared.clone();
        let task = tokio::spawn(async move {
            let read = async {
                let mut read = JsonlReader::new(read);
                while let Some(line) = read.next_line().await? {
                    let Ok(event) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if event.get("type").and_then(Value::as_str) == Some("response")
                        && let Some(id) = event.get("id").and_then(Value::as_str)
                    {
                        let pending = connection
                            .requests
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .pending
                            .remove(id);
                        if let Some(pending) = pending {
                            let response = serde_json::from_value(event)
                                .map_err(|error| Arc::from(error.to_string()));
                            let _ = pending.send(response);
                            continue;
                        }
                    }
                    let _ = connection.events.send(event);
                }
                Err::<(), _>(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Agent process closed stdout",
                ))
            };
            let write = async {
                while let Some(bytes) = frames.recv().await {
                    write.write_all(&bytes).await?;
                    write.flush().await?;
                }
                write.shutdown().await
            };
            let result = tokio::select! { result = read => result, result = write => result };
            connection.fail(
                result
                    .err()
                    .map_or_else(|| "RPC stream closed".into(), |error| error.to_string()),
            );
        });
        Self {
            outbound,
            shared,
            task,
        }
    }
}

impl RpcClient {
    pub fn new(options: RpcClientOptions) -> Self {
        Self {
            options,
            child: None,
            transport: None,
            stderr: Arc::new(Mutex::new(Vec::new())),
            stderr_task: None,
            counter: AtomicU64::new(0),
        }
    }

    /// Attach to connected streams without starting a process. Requires a Tokio runtime.
    pub fn from_stream(
        read: impl AsyncRead + Unpin + Send + 'static,
        write: impl AsyncWrite + Unpin + Send + 'static,
    ) -> Self {
        let mut client = Self::default();
        client.transport = Some(Transport::new(read, write));
        client
    }

    /// Start the native `pi --mode rpc` process.
    pub async fn start(&mut self) -> io::Result<()> {
        if self.transport.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Client already started",
            ));
        }
        let mut command = Command::new(
            self.options
                .cli_path
                .as_deref()
                .unwrap_or_else(|| std::path::Path::new("pi")),
        );
        command.args(["--mode", "rpc"]).envs(&self.options.env);
        if let Some(cwd) = &self.options.cwd {
            command.current_dir(cwd);
        }
        if let Some(provider) = self
            .options
            .provider
            .as_deref()
            .filter(|provider| !provider.is_empty())
        {
            command.args(["--provider", provider]);
        }
        if let Some(model) = self
            .options
            .model
            .as_deref()
            .filter(|model| !model.is_empty())
        {
            command.args(["--model", model]);
        }
        command
            .args(&self.options.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            io::Error::new(error.kind(), format!("Agent process error: {error}"))
        })?;
        let (Some(stdout), Some(stdin), Some(mut stderr)) =
            (child.stdout.take(), child.stdin.take(), child.stderr.take())
        else {
            return Err(io::Error::other("Missing agent process pipes"));
        };
        let captured = self.stderr.clone();
        self.stderr_task = Some(tokio::spawn(async move {
            let mut buffer = [0_u8; 4096];
            let mut console = tokio::io::stderr();
            while let Ok(len) = stderr.read(&mut buffer).await {
                if len == 0 {
                    break;
                }
                captured
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .extend_from_slice(&buffer[..len]);
                let _ = console.write_all(&buffer[..len]).await;
            }
        }));
        self.transport = Some(Transport::new(stdout, stdin));
        self.child = Some(child);
        sleep(Duration::from_millis(100)).await;
        if let Some(status) = self
            .child
            .as_mut()
            .map(Child::try_wait)
            .transpose()?
            .flatten()
        {
            let error = self.diagnostic(
                io::ErrorKind::BrokenPipe,
                &format!("Agent process exited ({status})"),
            );
            self.stop().await?;
            return Err(error);
        }
        Ok(())
    }

    pub async fn stop(&mut self) -> io::Result<()> {
        let result = if let Some(mut child) = self.child.take() {
            stop_child(&mut child).await
        } else {
            Ok(())
        };
        self.detach();
        if let Some(task) = self.stderr_task.take() {
            let _ = task.await;
        }
        result
    }

    fn detach(&mut self) {
        if let Some(transport) = self.transport.take() {
            transport.shared.fail("Client stopped".into());
            transport.task.abort();
        }
    }

    pub fn get_stderr(&self) -> String {
        String::from_utf8_lossy(
            &self
                .stderr
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        )
        .into_owned()
    }

    /// Subscribe to session events and extension UI requests. Unmatched responses are also events.
    pub fn on_event(&self) -> io::Result<broadcast::Receiver<Value>> {
        Ok(self.transport()?.shared.events.subscribe())
    }

    pub fn answer_ui(&self, response: &RpcExtensionUiResponse) -> io::Result<()> {
        self.send_frame(self.transport()?, response)
    }

    /// Send any supported command using the typed request enum.
    pub async fn request(&self, command: RpcCommand) -> io::Result<RpcResponse> {
        let transport = self.transport()?;
        let name = command.name();
        let id = format!("req_{}", self.counter.fetch_add(1, Ordering::Relaxed) + 1);
        let (send, receive) = oneshot::channel();
        {
            let mut requests = transport
                .shared
                .requests
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(error) = &requests.exit_error {
                return Err(self.diagnostic(io::ErrorKind::BrokenPipe, error));
            }
            requests.pending.insert(id.clone(), send);
        }
        let _pending = Pending {
            shared: &transport.shared,
            id: id.clone(),
        };
        self.send_frame(
            transport,
            &RpcRequest {
                id: Some(id),
                command,
            },
        )?;
        let response = timeout(RESPONSE_TIMEOUT, receive)
            .await
            .map_err(|_| {
                self.diagnostic(
                    io::ErrorKind::TimedOut,
                    &format!("Timeout waiting for response to {name}"),
                )
            })?
            .map_err(|_| self.diagnostic(io::ErrorKind::BrokenPipe, "RPC response channel closed"))?
            .map_err(|error| self.diagnostic(io::ErrorKind::BrokenPipe, &error))?;
        if !response.success {
            return Err(io::Error::other(
                response.error.unwrap_or_else(|| format!("{name} failed")),
            ));
        }
        Ok(response)
    }

    fn transport(&self) -> io::Result<&Transport> {
        self.transport
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "Client not started"))
    }

    fn send_frame(&self, transport: &Transport, value: &impl Serialize) -> io::Result<()> {
        transport
            .outbound
            .send(serialize_json_line(value)?)
            .map_err(|_| {
                self.diagnostic(
                    io::ErrorKind::BrokenPipe,
                    "Agent process stdin is not writable",
                )
            })
    }

    fn diagnostic(&self, kind: io::ErrorKind, message: &str) -> io::Error {
        io::Error::new(kind, format!("{message}. Stderr: {}", self.get_stderr()))
    }

    async fn data<T: DeserializeOwned>(&self, command: RpcCommand) -> io::Result<T> {
        let data = self.request(command).await?.data.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "RPC response has no data")
        })?;
        serde_json::from_value(data).map_err(io::Error::from)
    }

    async fn field<T: DeserializeOwned>(&self, command: RpcCommand, field: &str) -> io::Result<T> {
        let value = self
            .request(command)
            .await?
            .data
            .and_then(|mut data| data.as_object_mut()?.shift_remove(field))
            .unwrap_or(Value::Null);
        serde_json::from_value(value).map_err(io::Error::from)
    }

    pub async fn prompt(
        &self,
        message: impl Into<String>,
        images: Vec<ImageContent>,
        streaming_behavior: Option<RpcStreamingBehavior>,
    ) -> io::Result<RpcDisposition> {
        self.field(
            RpcCommand::Prompt {
                message: message.into(),
                images,
                streaming_behavior,
            },
            "disposition",
        )
        .await
    }

    pub async fn steer(
        &self,
        message: impl Into<String>,
        images: Vec<ImageContent>,
    ) -> io::Result<RpcDisposition> {
        self.field(
            RpcCommand::Steer {
                message: message.into(),
                images,
            },
            "disposition",
        )
        .await
    }

    pub async fn follow_up(
        &self,
        message: impl Into<String>,
        images: Vec<ImageContent>,
    ) -> io::Result<RpcDisposition> {
        self.field(
            RpcCommand::FollowUp {
                message: message.into(),
                images,
            },
            "disposition",
        )
        .await
    }

    pub async fn abort(&self) -> io::Result<()> {
        self.request(RpcCommand::Abort).await.map(|_| ())
    }
    pub async fn clear_queue(&self) -> io::Result<RpcQueue> {
        self.data(RpcCommand::ClearQueue).await
    }
    pub async fn new_session(&self, parent_session: Option<String>) -> io::Result<RpcCancelled> {
        self.data(RpcCommand::NewSession { parent_session }).await
    }
    pub async fn get_state(&self) -> io::Result<RpcSessionState> {
        self.data(RpcCommand::GetState).await
    }

    pub async fn set_model(
        &self,
        provider: impl Into<String>,
        model_id: impl Into<String>,
    ) -> io::Result<Model> {
        self.data(RpcCommand::SetModel {
            provider: provider.into(),
            model_id: model_id.into(),
        })
        .await
    }

    pub async fn cycle_model(&self) -> io::Result<Option<RpcModelCycle>> {
        self.data(RpcCommand::CycleModel).await
    }
    pub async fn get_available_models(&self) -> io::Result<Vec<Model>> {
        self.field(RpcCommand::GetAvailableModels, "models").await
    }
    pub async fn set_thinking_level(&self, level: ThinkingLevel) -> io::Result<()> {
        self.request(RpcCommand::SetThinkingLevel { level })
            .await
            .map(|_| ())
    }
    pub async fn cycle_thinking_level(&self) -> io::Result<Option<ThinkingLevel>> {
        self.field(RpcCommand::CycleThinkingLevel, "level").await
    }
    pub async fn get_available_thinking_levels(&self) -> io::Result<Vec<ThinkingLevel>> {
        self.field(RpcCommand::GetAvailableThinkingLevels, "levels")
            .await
    }
    pub async fn set_steering_mode(&self, mode: QueueMode) -> io::Result<()> {
        self.request(RpcCommand::SetSteeringMode { mode })
            .await
            .map(|_| ())
    }
    pub async fn set_follow_up_mode(&self, mode: QueueMode) -> io::Result<()> {
        self.request(RpcCommand::SetFollowUpMode { mode })
            .await
            .map(|_| ())
    }
    pub async fn compact(
        &self,
        custom_instructions: Option<String>,
    ) -> io::Result<RpcCompactionResult> {
        self.data(RpcCommand::Compact {
            custom_instructions,
        })
        .await
    }
    pub async fn set_auto_compaction(&self, enabled: bool) -> io::Result<()> {
        self.request(RpcCommand::SetAutoCompaction { enabled })
            .await
            .map(|_| ())
    }
    pub async fn set_auto_retry(&self, enabled: bool) -> io::Result<()> {
        self.request(RpcCommand::SetAutoRetry { enabled })
            .await
            .map(|_| ())
    }
    pub async fn abort_retry(&self) -> io::Result<()> {
        self.request(RpcCommand::AbortRetry).await.map(|_| ())
    }

    pub async fn bash(
        &self,
        command: impl Into<String>,
        exclude_from_context: Option<bool>,
    ) -> io::Result<RpcBashResult> {
        self.data(RpcCommand::Bash {
            command: command.into(),
            exclude_from_context,
        })
        .await
    }

    pub async fn abort_bash(&self) -> io::Result<()> {
        self.request(RpcCommand::AbortBash).await.map(|_| ())
    }
    pub async fn get_session_stats(&self) -> io::Result<RpcSessionStats> {
        self.data(RpcCommand::GetSessionStats).await
    }
    pub async fn export_html(&self, output_path: Option<String>) -> io::Result<String> {
        self.field(RpcCommand::ExportHtml { output_path }, "path")
            .await
    }

    pub async fn switch_session(
        &self,
        session_path: impl Into<String>,
    ) -> io::Result<RpcCancelled> {
        self.data(RpcCommand::SwitchSession {
            session_path: session_path.into(),
        })
        .await
    }

    pub async fn fork(&self, entry_id: impl Into<String>) -> io::Result<RpcForkResult> {
        self.data(RpcCommand::Fork {
            entry_id: entry_id.into(),
        })
        .await
    }
    pub async fn clone_session(&self) -> io::Result<RpcCancelled> {
        self.data(RpcCommand::Clone).await
    }
    pub async fn get_fork_messages(&self) -> io::Result<Vec<RpcForkMessage>> {
        self.field(RpcCommand::GetForkMessages, "messages").await
    }
    pub async fn get_entries(&self, since: Option<String>) -> io::Result<RpcEntries> {
        self.data(RpcCommand::GetEntries { since }).await
    }
    pub async fn get_tree(&self) -> io::Result<RpcTree> {
        self.data(RpcCommand::GetTree).await
    }
    pub async fn get_last_assistant_text(&self) -> io::Result<Option<String>> {
        self.field(RpcCommand::GetLastAssistantText, "text").await
    }
    pub async fn set_session_name(&self, name: impl Into<String>) -> io::Result<()> {
        self.request(RpcCommand::SetSessionName { name: name.into() })
            .await
            .map(|_| ())
    }
    pub async fn get_messages(&self) -> io::Result<Vec<AgentMessage>> {
        self.field(RpcCommand::GetMessages, "messages").await
    }
    pub async fn get_commands(&self) -> io::Result<Vec<RpcSlashCommand>> {
        self.field(RpcCommand::GetCommands, "commands").await
    }

    pub async fn wait_for_idle(&self, max_wait: Duration) -> io::Result<()> {
        self.until_settled(self.on_event()?, max_wait, |_| {}).await
    }

    pub async fn collect_events(&self, max_wait: Duration) -> io::Result<Vec<Value>> {
        let mut events = Vec::new();
        self.until_settled(self.on_event()?, max_wait, |event| events.push(event))
            .await?;
        Ok(events)
    }

    pub async fn prompt_and_wait(
        &self,
        message: impl Into<String>,
        images: Vec<ImageContent>,
        max_wait: Duration,
    ) -> io::Result<Vec<Value>> {
        let mut receiver = self.on_event()?;
        let disposition = self.prompt(message, images, None).await?;
        let mut events = Vec::new();
        if disposition == RpcDisposition::Handled {
            while let Ok(event) = receiver.try_recv() {
                events.push(event);
            }
        } else {
            self.until_settled(receiver, max_wait, |event| events.push(event))
                .await?;
        }
        Ok(events)
    }

    async fn until_settled(
        &self,
        mut events: broadcast::Receiver<Value>,
        max_wait: Duration,
        mut collect: impl FnMut(Value),
    ) -> io::Result<()> {
        let shared = &self.transport()?.shared;
        let closed = shared.closed.notified();
        tokio::pin!(closed);
        closed.as_mut().enable();
        let receive = async {
            loop {
                let event = events.recv().await.map_err(io::Error::other)?;
                let settled = event.get("type").and_then(Value::as_str) == Some("agent_settled");
                collect(event);
                if settled {
                    return Ok(());
                }
            }
        };
        let status = async {
            if shared
                .requests
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .exit_error
                .is_none()
            {
                closed.await;
            }
            Err(self.diagnostic(io::ErrorKind::BrokenPipe, "RPC stream closed"))
        };
        timeout(max_wait, async {
            tokio::select! { biased; result = receive => result, result = status => result }
        })
        .await
        .map_err(|_| {
            self.diagnostic(
                io::ErrorKind::TimedOut,
                "Timeout waiting for agent to become idle",
            )
        })?
    }
}

impl Default for RpcClient {
    fn default() -> Self {
        Self::new(RpcClientOptions::default())
    }
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        self.detach();
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
    }
}

async fn stop_child(child: &mut Child) -> io::Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    #[cfg(unix)]
    if let Some(pid) = child.id().and_then(|pid| i32::try_from(pid).ok()) {
        use nix::{
            sys::signal::{Signal, kill},
            unistd::Pid,
        };
        let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
    }
    #[cfg(not(unix))]
    child.start_kill()?;
    match timeout(Duration::from_secs(1), child.wait()).await {
        Ok(result) => result.map(|_| ()),
        Err(_) => child.kill().await,
    }
}
