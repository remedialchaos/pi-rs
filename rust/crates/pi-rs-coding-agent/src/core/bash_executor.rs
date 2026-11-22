//! Bash execution with streaming, cancellation and truncation. Ported from
//! `core/bash-executor.ts` onto the local process executor in `core/tools/bash.rs`.
//!
//! Output is sanitized once per chunk (ANSI stripped, binary garbage replaced,
//! CR discarded), kept in a rolling buffer, and tail-truncated through
//! `core::tools::truncate::truncate_tail`. When output exceeds the buffer the
//! complete sanitized text is persisted to a temp file.

use std::io::Write as _;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;

use pi_rs_agent::CancellationToken;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::{Duration, sleep, timeout};

use super::tools::bash::EXIT_STDIO_GRACE_MS;
use super::tools::truncate::{DEFAULT_MAX_BYTES, TruncationOptions, truncate_tail};
use crate::utils::ansi::{split_incomplete_ansi_suffix, strip_ansi};
use crate::utils::output_files::create_output_file_stream;
use crate::utils::shell::sanitize_binary_output;

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

fn push_rolling(chunks: &mut Vec<String>, bytes: &mut usize, discarded: &mut bool, text: &str) {
    if text.is_empty() {
        return;
    }
    chunks.push(text.to_string());
    *bytes += text.len();
    while *bytes > MAX_OUTPUT_BYTES && chunks.len() > 1 {
        let removed = chunks.remove(0);
        *bytes = bytes.saturating_sub(removed.len());
        *discarded = true;
    }
}

fn shell_command() -> (String, Vec<&'static str>) {
    #[cfg(windows)]
    {
        ("bash".to_string(), vec!["-c"])
    }
    #[cfg(not(windows))]
    {
        if Path::new("/bin/bash").exists() {
            ("/bin/bash".to_string(), vec!["-c"])
        } else {
            ("sh".to_string(), vec!["-c"])
        }
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
        let (shell, args) = shell_command();
        let mut child = Command::new(shell)
            .args(args)
            .arg(command)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("Failed to spawn bash: {error}"))?;

        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let mut stdout_task = tokio::spawn(async move {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = stdout_pipe {
                let _ = pipe.read_to_end(&mut bytes).await;
            }
            bytes
        });
        let mut stderr_task = tokio::spawn(async move {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = stderr_pipe {
                let _ = pipe.read_to_end(&mut bytes).await;
            }
            bytes
        });

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
        let outcome = tokio::select! {
            result = child.wait() => WaitOutcome::Status(result),
            _ = timeout_future => WaitOutcome::TimedOut,
            _ = cancel_future => WaitOutcome::Cancelled,
        };
        let mut timed_out = matches!(outcome, WaitOutcome::TimedOut);
        let mut cancelled = matches!(outcome, WaitOutcome::Cancelled);
        if !matches!(outcome, WaitOutcome::Status(_)) {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        let status = match outcome {
            WaitOutcome::Status(result) => {
                Some(result.map_err(|error| format!("Failed to wait for bash: {error}"))?)
            }
            WaitOutcome::TimedOut | WaitOutcome::Cancelled => None,
        };

        let drain = Duration::from_millis(EXIT_STDIO_GRACE_MS);
        let stdout_bytes = timeout(drain, &mut stdout_task)
            .await
            .ok()
            .and_then(|result| result.ok())
            .unwrap_or_default();
        let stderr_bytes = timeout(drain, &mut stderr_task)
            .await
            .ok()
            .and_then(|result| result.ok())
            .unwrap_or_default();

        let mut combined = stdout_bytes;
        combined.extend_from_slice(&stderr_bytes);
        let full_output = String::from_utf8_lossy(&combined).into_owned();

        let mut pending = String::new();
        let mut chunks = Vec::new();
        let mut chunk_bytes = 0;
        let mut discarded = false;
        let text = sanitize_chunk(&full_output, &mut pending);
        push_rolling(&mut chunks, &mut chunk_bytes, &mut discarded, &text);
        flush_pending(&mut pending, &options.on_chunk);
        if let Some(callback) = &options.on_chunk {
            callback(&text);
        }

        let sanitized = chunks.join("");
        let snapshot = truncate_tail(&sanitized, &TruncationOptions::default());
        let mut full_output_path = None;
        if (discarded || snapshot.truncated)
            && let Ok((path, mut file)) = create_output_file_stream("pi-bash", ".log")
            && file.write_all(sanitized.as_bytes()).is_ok()
        {
            full_output_path = Some(path.to_string_lossy().into_owned());
        }
        if options
            .cancel
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            cancelled = true;
            timed_out = false;
        }
        Ok(BashOutput {
            output: if snapshot.truncated {
                snapshot.content
            } else {
                sanitized
            },
            exit_code: if cancelled || timed_out {
                None
            } else {
                status.and_then(|status| status.code())
            },
            cancelled,
            truncated: discarded || snapshot.truncated,
            full_output_path,
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
        let mut chunks = Vec::new();
        let mut bytes = 0;
        let mut discarded = false;
        let payload = "x".repeat(MAX_OUTPUT_BYTES + 1);
        push_rolling(&mut chunks, &mut bytes, &mut discarded, &payload);
        assert!(!discarded);
        push_rolling(&mut chunks, &mut bytes, &mut discarded, "tail");
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
