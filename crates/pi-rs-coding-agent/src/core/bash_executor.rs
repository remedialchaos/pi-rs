//! Bash execution with streaming, cancellation and truncation. Ported from
//! `core/bash-executor.ts` onto the local process executor in `core/tools/bash.rs`.
//!
//! Output is sanitized once per chunk (ANSI stripped, binary garbage replaced,
//! CR discarded), kept in a rolling buffer, and tail-truncated through
//! `core::tools::truncate::truncate_tail`. When output exceeds the buffer the
//! complete sanitized text is persisted to a temp file.

use pi_rs_agent::CancellationToken;
use std::{
    collections::VecDeque,
    fs::File,
    io::Write as _,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::Arc,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::{Duration, sleep, timeout};

use super::tools::bash::EXIT_STDIO_GRACE_MS;
use super::tools::truncate::{DEFAULT_MAX_BYTES, TruncationOptions, truncate_tail};
use crate::utils::ansi::{split_incomplete_ansi_suffix, strip_ansi};
use crate::utils::output_files::create_output_file_stream;
use crate::utils::shell::{CommandTransport, get_shell_config, sanitize_binary_output};

/// Maximum number of sanitized bytes retained in the rolling buffer.
pub const MAX_OUTPUT_BYTES: usize = DEFAULT_MAX_BYTES * 2;

/// Callback receiving each sanitized output chunk.
pub type ChunkCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// Callbacks and cancellation for one execution.
#[derive(Clone, Default)]
pub struct BashExecutorOptions {
    pub timeout_ms: Option<u64>,
    pub cancel: Option<CancellationToken>,
    pub on_chunk: Option<ChunkCallback>,
    pub shell_path: Option<String>,
}

/// Result of a bash execution.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BashOutput {
    /// Combined stdout + stderr, sanitized and possibly truncated.
    pub output: String,
    /// Process exit code; `None` when cancelled, timed out, or killed.
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    /// Path to the temp file holding the full output when it was truncated.
    pub full_output_path: Option<String>,
}

/// Stateless executor for `!` commands and direct mode callers.
#[derive(Clone, Copy, Debug, Default)]
pub struct BashExecutor;

/// Sanitize one decoded chunk. An incomplete ANSI sequence at the end is
/// carried in `pending` and completed by the next chunk.
pub fn sanitize_chunk(raw: &str, pending: &mut String) -> String {
    let combined = format!("{pending}{raw}");
    let (complete, rest) = split_incomplete_ansi_suffix(&combined);
    *pending = rest;
    sanitize_binary_output(&strip_ansi(&complete)).replace('\r', "")
}

/// Flush a pending ANSI suffix at end of stream.
pub fn flush_pending(pending: &mut String, on_chunk: &Option<ChunkCallback>) -> String {
    if pending.is_empty() {
        return String::new();
    }
    let text = sanitize_binary_output(&strip_ansi(pending)).replace('\r', "");
    *pending = String::new();
    if !text.is_empty()
        && let Some(callback) = on_chunk
    {
        callback(&text);
    }
    text
}

fn push_rolling(
    chunks: &mut VecDeque<String>,
    bytes: &mut usize,
    discarded: &mut bool,
    text: String,
) {
    *bytes += text.len();
    chunks.push_back(text);
    while *bytes > MAX_OUTPUT_BYTES && chunks.len() > 1 {
        if let Some(removed) = chunks.pop_front() {
            *bytes -= removed.len();
            *discarded = true;
        }
    }
}

#[derive(Default)]
struct Capture {
    chunks: VecDeque<String>,
    raw_bytes: usize,
    bytes: usize,
    discarded: bool,
    ansi: String,
    file: Option<(PathBuf, File)>,
}

impl Capture {
    fn ensure_file(&mut self) -> Result<(), String> {
        if self.file.is_none() {
            let (path, mut file) =
                create_output_file_stream("pi-bash", ".log").map_err(|error| error.to_string())?;
            for chunk in &self.chunks {
                file.write_all(chunk.as_bytes())
                    .map_err(|error| error.to_string())?;
            }
            self.file = Some((path, file));
        }
        Ok(())
    }

    fn push(&mut self, text: String, callback: &Option<ChunkCallback>) -> Result<(), String> {
        if text.is_empty() {
            return Ok(());
        }
        if self.raw_bytes > DEFAULT_MAX_BYTES {
            self.ensure_file()?;
        }
        if let Some((_, file)) = &mut self.file {
            file.write_all(text.as_bytes())
                .map_err(|error| error.to_string())?;
        }
        if let Some(callback) = callback {
            callback(&text);
        }
        push_rolling(&mut self.chunks, &mut self.bytes, &mut self.discarded, text);
        Ok(())
    }

    fn ingest(&mut self, bytes: &[u8], callback: &Option<ChunkCallback>) -> Result<(), String> {
        self.raw_bytes += bytes.len();
        let valid = String::from_utf8_lossy(bytes);
        let text = sanitize_chunk(&valid, &mut self.ansi);
        self.push(text, callback)
    }
}

struct Pipe<R> {
    reader: Option<R>,
    buf: [u8; 8192],
    pending: usize,
}

impl<R: AsyncRead + Unpin> Pipe<R> {
    fn new(reader: Option<R>) -> Self {
        Self {
            reader,
            buf: [0; 8192],
            pending: 0,
        }
    }

    fn ingest(
        &mut self,
        read: usize,
        capture: &mut Capture,
        callback: &Option<ChunkCallback>,
    ) -> Result<(), String> {
        let end = self.pending + read;
        if read == 0 {
            self.reader = None;
            self.pending = 0;
            return capture.ingest(&self.buf[..end], callback);
        }
        let mut complete = end;
        // A UTF-8 scalar may straddle pipe reads. Carry its prefix, not the preceding data.
        if let Some(start) = self.buf[..end]
            .iter()
            .rposition(|&byte| byte & 0xc0 != 0x80)
            && end - start < 4
            && std::str::from_utf8(&self.buf[start..end])
                .is_err_and(|error| error.error_len().is_none())
        {
            complete = start;
        }
        capture.ingest(&self.buf[..complete], callback)?;
        self.buf.copy_within(complete..end, 0);
        self.pending = end - complete;
        Ok(())
    }

    async fn read_chunk(&mut self) -> Result<usize, String> {
        let Some(reader) = &mut self.reader else {
            return Ok(0);
        };
        reader
            .read(&mut self.buf[self.pending..])
            .await
            .map_err(|error| error.to_string())
    }
}

struct ChildTree(Option<u32>);

impl ChildTree {
    fn kill(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0.take().and_then(|pid| i32::try_from(pid).ok()) {
            use nix::{
                sys::signal::{Signal, killpg},
                unistd::Pid,
            };
            let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
        }
        #[cfg(windows)]
        if let Some(pid) = self.0.take() {
            let _ = crate::utils::child_process::kill_process_tree(pid);
        }
    }
}

impl Drop for ChildTree {
    fn drop(&mut self) {
        self.kill();
    }
}

enum WaitOutcome {
    Status(std::io::Result<ExitStatus>),
    TimedOut,
    Cancelled,
}

impl BashExecutor {
    pub fn new() -> Self {
        Self
    }

    /// Execute `command` in `cwd`, returning sanitized/aggregated output.
    pub async fn execute(
        &self,
        command: &str,
        cwd: &Path,
        options: &BashExecutorOptions,
    ) -> Result<BashOutput, String> {
        if !cwd.exists() {
            return Err(format!(
                "Working directory does not exist: {}\nCannot execute bash commands.",
                cwd.display()
            ));
        }
        if options
            .cancel
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Ok(BashOutput {
                cancelled: true,
                ..BashOutput::default()
            });
        }
        let shell = get_shell_config(options.shell_path.as_deref())?;
        let mut process = Command::new(shell.shell);
        process
            .args(shell.args)
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        match shell.command_transport {
            CommandTransport::Argv => {
                process.arg(command).stdin(Stdio::null());
            }
            CommandTransport::Stdin => {
                process.stdin(Stdio::piped());
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            process.as_std_mut().process_group(0);
        }
        let mut child = process
            .spawn()
            .map_err(|error| format!("Failed to spawn bash: {error}"))?;
        let mut children = ChildTree(child.id());
        let mut stdout = Pipe::new(child.stdout.take());
        let mut stderr = Pipe::new(child.stderr.take());
        let mut stdin = child.stdin.take();
        let input = async {
            if let Some(stdin) = &mut stdin {
                let _ = stdin.write_all(command.as_bytes()).await;
                let _ = stdin.shutdown().await;
            }
            std::future::pending::<()>().await;
        };

        let timeout_future = async {
            match options.timeout_ms {
                Some(millis) => {
                    sleep(Duration::from_millis(millis)).await;
                    true
                }
                None => std::future::pending::<bool>().await,
            }
        };
        let cancel_future = async {
            match &options.cancel {
                Some(token) => {
                    token.cancelled().await;
                    true
                }
                None => std::future::pending::<bool>().await,
            }
        };
        let mut capture = Capture::default();
        let outcome = {
            let pump = async {
                while stdout.reader.is_some() || stderr.reader.is_some() {
                    tokio::select! {
                        read = stdout.read_chunk(), if stdout.reader.is_some() => stdout.ingest(read?, &mut capture, &options.on_chunk)?,
                        read = stderr.read_chunk(), if stderr.reader.is_some() => stderr.ingest(read?, &mut capture, &options.on_chunk)?,
                    }
                }
                Ok::<_, String>(())
            };
            let wait = async {
                tokio::select! {
                    result = child.wait() => WaitOutcome::Status(result),
                    _ = timeout_future => {
                        children.kill();
                        let _ = child.kill().await;
                        WaitOutcome::TimedOut
                    }
                    _ = cancel_future => {
                        children.kill();
                        let _ = child.kill().await;
                        WaitOutcome::Cancelled
                    }
                    () = input => WaitOutcome::Cancelled,
                }
            };
            tokio::pin!(pump, wait);
            tokio::select! {
                outcome = &mut wait => {
                    if let Ok(result) = timeout(Duration::from_millis(EXIT_STDIO_GRACE_MS), &mut pump).await {
                        result?;
                    }
                    outcome
                }
                result = &mut pump => { result?; wait.await }
            }
        };
        if matches!(&outcome, WaitOutcome::Status(_)) {
            // A successful shell may have intentionally detached a child; cancellation alone
            // should terminate the process tree.
            children.0 = None;
        }
        let pending = flush_pending(&mut capture.ansi, &None);
        capture.push(pending, &options.on_chunk)?;
        let mut output = String::with_capacity(capture.bytes);
        for chunk in &capture.chunks {
            output.push_str(chunk);
        }
        let snapshot = truncate_tail(&output, &TruncationOptions::default());
        if snapshot.truncated {
            capture.ensure_file()?;
        }
        let cancelled = matches!(&outcome, WaitOutcome::Cancelled)
            || options
                .cancel
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled);
        let exit_code = match outcome {
            WaitOutcome::Status(status) => status
                .map_err(|error| format!("Failed to wait for bash: {error}"))?
                .code(),
            WaitOutcome::TimedOut | WaitOutcome::Cancelled => None,
        };
        Ok(BashOutput {
            output: if snapshot.truncated {
                snapshot.content
            } else {
                output
            },
            exit_code: exit_code.filter(|_| !cancelled),
            cancelled,
            truncated: capture.discarded || snapshot.truncated,
            full_output_path: capture
                .file
                .map(|(path, _)| path.to_string_lossy().into_owned()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_ansi_binary_and_carriage_returns() {
        let mut pending = String::new();
        assert_eq!(
            sanitize_chunk("\u{1b}[31mred\u{1b}[0m\r\n", &mut pending),
            "red\n"
        );
    }

    #[test]
    fn carries_incomplete_ansi_sequence_across_chunks() {
        let mut pending = String::new();
        assert_eq!(sanitize_chunk("before\u{1b}[3", &mut pending), "before");
        assert_eq!(pending, "\u{1b}[3");
        assert_eq!(sanitize_chunk("1mafter", &mut pending), "after");
        assert!(pending.is_empty());
    }

    #[test]
    fn rolling_buffer_discards_oldest_chunks() {
        let mut chunks = VecDeque::new();
        let mut bytes = 0;
        let mut discarded = false;
        let payload = "x".repeat(MAX_OUTPUT_BYTES + 1);
        push_rolling(&mut chunks, &mut bytes, &mut discarded, payload);
        assert!(!discarded);
        push_rolling(&mut chunks, &mut bytes, &mut discarded, "tail".into());
        assert!(discarded);
    }

    #[tokio::test]
    async fn captures_stdout_and_exit_code() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let output = BashExecutor::new()
            .execute("printf hello", dir.path(), &BashExecutorOptions::default())
            .await?;
        assert_eq!(output.output, "hello");
        assert_eq!(output.exit_code, Some(0));
        assert!(!output.truncated);
        Ok(())
    }

    #[tokio::test]
    async fn captures_stderr_and_nonzero_exit_code() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let output = BashExecutor::new()
            .execute(
                "echo boom >&2; exit 3",
                dir.path(),
                &BashExecutorOptions::default(),
            )
            .await?;
        assert_eq!(output.output, "boom\n");
        assert_eq!(output.exit_code, Some(3));
        Ok(())
    }

    #[tokio::test]
    async fn truncated_output_is_persisted() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let command = format!(
            "head -c {} /dev/zero | tr '\\0' 'x'",
            DEFAULT_MAX_BYTES + 1024
        );
        let output = BashExecutor::new()
            .execute(&command, dir.path(), &BashExecutorOptions::default())
            .await?;
        assert!(output.truncated);
        assert!(output.output.len() <= DEFAULT_MAX_BYTES);
        let path = output.full_output_path.ok_or("no full output path")?;
        assert!(std::path::Path::new(&path).exists());
        std::fs::remove_file(&path).map_err(|e| e.to_string())?;
        Ok(())
    }

    #[tokio::test]
    async fn pre_cancelled_execution_reports_cancelled() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let token = CancellationToken::new();
        token.cancel();
        let output = BashExecutor::new()
            .execute(
                "sleep 2",
                dir.path(),
                &BashExecutorOptions {
                    cancel: Some(token),
                    ..BashExecutorOptions::default()
                },
            )
            .await?;
        assert!(output.cancelled);
        assert_eq!(output.exit_code, None);
        Ok(())
    }

    #[tokio::test]
    async fn streaming_callback_receives_sanitized_text() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let received = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        let output = BashExecutor::new()
            .execute(
                "printf 'a\\rb'",
                dir.path(),
                &BashExecutorOptions {
                    on_chunk: Some(Arc::new(move |chunk| {
                        if let Ok(mut values) = sink.lock() {
                            values.push(chunk.to_string());
                        }
                    })),
                    ..BashExecutorOptions::default()
                },
            )
            .await?;
        assert_eq!(output.output, "ab");
        assert_eq!(received.lock().map_err(|e| e.to_string())?.join(""), "ab");
        Ok(())
    }
}
