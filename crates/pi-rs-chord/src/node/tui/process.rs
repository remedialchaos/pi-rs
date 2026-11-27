use super::*;
use pi_rs_tui::terminal::{ProcessTerminal, Terminal, TerminalEvent};
use std::{collections::VecDeque, process::Stdio, time::Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Trusted host UI for local actions. Implementations must obtain confirmation
/// using their own UI before acting on sandbox-controlled values.
pub trait LocalActions {
    fn copy(&mut self, text: &str) -> Result<bool>;
    fn open(&mut self, url: &http::Uri) -> Result<bool>;
    fn attach(&mut self, path: &Path) -> Result<Option<(String, Vec<u8>)>>;
}

fn local_reply(
    id: u64,
    request: &BridgeRequest,
    actions: &mut impl LocalActions,
) -> Result<LauncherMessage> {
    let value = match request {
        BridgeRequest::Copy { text } => Value::Bool(actions.copy(text)?),
        BridgeRequest::Open { url } => {
            let uri: http::Uri = url
                .parse()
                .map_err(|_| Error::Invalid("invalid URL".into()))?;
            if !matches!(uri.scheme_str(), Some("https" | "http"))
                || uri.authority().is_none_or(|a| a.as_str().contains('@'))
            {
                return Err(Error::Invalid("invalid URL".into()));
            }
            Value::Bool(actions.open(&uri)?)
        }
        BridgeRequest::Attach { path } => {
            if let Some((name, bytes)) = actions.attach(Path::new(path))? {
                if bytes.len() > 16 * 1024 * 1024 {
                    return Err(Error::Invalid("attachment too large".into()));
                }
                return Ok(LauncherMessage::Attached { id, name, bytes });
            }
            Value::Null
        }
        _ => return Err(Error::Invalid("unknown local action".into())),
    };
    Ok(LauncherMessage::Reply { id, value })
}

fn interrupted(keys: &str, recent: &mut VecDeque<Instant>, now: Instant) -> bool {
    recent.retain(|time| now.duration_since(*time) < Duration::from_secs(1));
    for _ in keys.bytes().filter(|b| *b == 3) {
        recent.push_back(now);
        if recent.len() == 3 {
            return true;
        }
    }
    false
}

impl<G: SurfaceGet> TerminalSession<G> {
    /// Launch an operator-supplied sandbox executable. Its stdin receives the
    /// bridge start frame followed by launcher messages. The downloaded script
    /// is data in the start frame. The executable owns guest isolation.
    pub async fn run_subprocess(
        self,
        executable: &Path,
        script: &[u8],
        actions: &mut impl LocalActions,
    ) -> Result<i32> {
        self.run_with_terminal(
            tokio::process::Command::new(executable),
            script,
            actions,
            ProcessTerminal::new(),
        )
        .await
    }

    /// Drive an alternate terminal transport, retaining the subprocess protocol
    /// and local action checks used by the system terminal.
    pub async fn run_with_terminal(
        mut self,
        mut command: tokio::process::Command,
        script: &[u8],
        actions: &mut impl LocalActions,
        mut terminal: impl Terminal,
    ) -> Result<i32> {
        let mut child = command
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let mut input = child.stdin.take().ok_or(Error::Closed)?;
        let mut output = child.stdout.take().ok_or(Error::Closed)?;
        let (replies, mut outgoing) = mpsc::channel::<LauncherMessage>(QUEUE_LENGTH);
        self.replies = Some(replies.clone());
        let start = frame(
            &serde_json::json!({"kind":"start","start":self.channel.start()}),
            script,
        )?;
        let mut writer = tokio::spawn(async move {
            input.write_all(&start).await?;
            while let Some(message) = outgoing.recv().await {
                input.write_all(&launcher_frame(&message)?).await?;
            }
            Ok::<_, Error>(())
        });
        let reset = "\x1b[?2026l\x1b[?1000l\x1b[?1002l\x1b[?1006l\x1b[?1004l\x1b[?2004l\x1b[?25h\x1b[r\x1b[?7h\x1b[?1049l\x1b[0m";
        terminal.set_exit_restore(reset, "");
        let result = async {
            terminal.start_session()?;
            let mut reader = FrameReader::default();
            let mut buffer = [0; 8192];
            let mut tick = tokio::time::interval(Duration::from_millis(10));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            #[cfg(unix)]
            let mut resize = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
            let mut recent = VecDeque::new();
            loop {
                tokio::select! {
                    done = &mut writer => {
                        done.map_err(|e| Error::Invalid(e.to_string()))??;
                        return Err(Error::Closed);
                    }
                    _ = async {
                        #[cfg(unix)]
                        if resize.recv().await.is_some() { return; }
                        std::future::pending::<()>().await
                    } => {
                        replies.try_send(LauncherMessage::Resize {
                            columns: terminal.width(), rows: terminal.height(),
                        }).map_err(|_| Error::Closed)?;
                    }
                    _ = tick.tick() => {
                        for event in terminal.poll_input(Duration::ZERO)? {
                            let message = match event {
                                TerminalEvent::Input(data) => {
                                    let keys = if pi_rs_tui::keys::matches_key_data(&data, "ctrl+c") { "\x03" } else { &data };
                                    if interrupted(keys, &mut recent, Instant::now()) {
                                        return Ok(130);
                                    }
                                    LauncherMessage::Input { data }
                                }
                                TerminalEvent::Resize { columns, rows } => LauncherMessage::Resize { columns, rows },
                            };
                            replies.try_send(message).map_err(|_| Error::Closed)?;
                        }
                    }
                    length = output.read(&mut buffer) => {
                        let length = length?;
                        if length == 0 { reader.finish()?; return Ok(tokio::time::timeout(NETWORK_TIMEOUT,child.wait()).await.map_err(|_|Error::Closed)??.code().unwrap_or(1)); }
                        for message in reader.push(&buffer[..length])? {
                            if let SandboxMessage::Request { id, request: request @ (BridgeRequest::Copy { .. } | BridgeRequest::Open { .. } | BridgeRequest::Attach { .. }) } = &message {
                                let reply = match local_reply(*id, request, actions) {
                                    Ok(reply) => reply,
                                    Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::Interrupted => return Ok(130),
                                    Err(_) => LauncherMessage::Refused { id: *id, message: "terminal request refused".into() },
                                };
                                replies.try_send(reply).map_err(|_| Error::Closed)?;
                            } else {
                                let mut rendered = Vec::new();
                                let exit = self.handle(Some(message), &mut rendered).await?;
                                if !rendered.is_empty() {
                                    terminal.write(std::str::from_utf8(&rendered).map_err(|_| Error::Invalid("invalid rendered output".into()))?);
                                }
                                if let Some(code) = exit { return Ok(code); }
                            }
                        }
                    }
                }
            }
        }.await;
        writer.abort();
        self.close();
        let _ = child.kill().await;
        terminal.write(reset);
        terminal.stop_session()?;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn third_interrupt_exits_only_within_one_second() {
        let start = Instant::now();
        let mut recent = VecDeque::new();
        assert!(!interrupted("\x03\x03", &mut recent, start));
        let later = start + Duration::from_secs(1);
        assert!(!interrupted("\x03", &mut recent, later));
        assert!(interrupted("x\x03\x03", &mut recent, later));
    }
}
