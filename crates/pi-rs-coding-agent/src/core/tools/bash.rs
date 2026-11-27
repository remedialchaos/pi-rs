//! Shell execution tool ported from `core/tools/bash.ts`.
//!
//! The local backend captures stdout and stderr, applies tail truncation, and
//! reports non-zero exits as error results. `powershell.rs` reuses the same
//! executor with different shell arguments.

use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use pi_rs_agent::CancellationToken;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::time::{Duration, timeout};

use super::truncate::{
    DEFAULT_MAX_BYTES, TruncationOptions, TruncationResult, format_size, truncate_tail,
};

pub type BashChunkCallback = Arc<dyn Fn(&str) + Send + Sync>;

pub const EXIT_STDIO_GRACE_MS: u64 = 100;
pub const MAX_TIMEOUT_MS: u64 = 2_147_483_647;
pub const STRUCTURED_OUTPUT_MAX_BYTES: usize = 1024 * 1024;

/// Non-interactive environment defaults applied to spawned shell commands so
/// pagers, editors, and credential prompts fail cleanly instead of hanging.
/// Inline command assignments (e.g. `PAGER=more ...`) still take precedence.
pub const NON_INTERACTIVE_ENV: &[(&str, &str)] = &[
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("MANPAGER", "cat"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_EDITOR", "true"),
    ("EDITOR", "true"),
    ("VISUAL", "true"),
    ("GH_PROMPT_DISABLED", "1"),
    ("SSH_ASKPASS", "false"),
    ("SSH_ASKPASS_REQUIRE", "force"),
    ("DEBIAN_FRONTEND", "noninteractive"),
    ("PIP_NO_INPUT", "1"),
];

#[derive(Clone, Debug, Deserialize)]
pub struct BashInput {
    pub command: String,
    #[serde(default)]
    pub timeout: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BashOutput {
    pub output: String,
    pub truncated: bool,
    pub full_output_path: Option<String>,
    pub exit_code: i32,
    pub wall_time_seconds: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BashDetails {
    pub truncation: Option<TruncationResult>,
    pub full_output_path: Option<String>,
}

#[derive(Clone, Debug)]
pub struct BashResult {
    pub text: String,
    pub details: Option<BashDetails>,
    pub structured: BashOutput,
    pub is_error: bool,
}

pub fn resolve_timeout_ms(timeout_seconds: Option<f64>) -> Result<Option<u64>, String> {
    let Some(seconds) = timeout_seconds else {
        return Ok(None);
    };
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err("Invalid timeout: must be a finite number of seconds".to_string());
    }
    let millis = seconds * 1000.0;
    if millis > MAX_TIMEOUT_MS as f64 {
        return Err(format!(
            "Invalid timeout: maximum is {} seconds",
            MAX_TIMEOUT_MS as f64 / 1000.0
        ));
    }
    Ok(Some(millis as u64))
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut boundary = index.min(text.len());
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
}

fn ceil_char_boundary(text: &str, index: usize) -> usize {
    let mut boundary = index.min(text.len());
    while boundary < text.len() && !text.is_char_boundary(boundary) {
        boundary += 1;
    }
    boundary
}

fn structured_output(full_output: &str) -> (String, bool) {
    if full_output.len() <= STRUCTURED_OUTPUT_MAX_BYTES {
        return (full_output.to_string(), false);
    }
    let half = STRUCTURED_OUTPUT_MAX_BYTES / 2;
    let head_end = ceil_char_boundary(full_output, half);
    let tail_start = floor_char_boundary(full_output, full_output.len() - half);
    let omitted = full_output.len() - head_end - (full_output.len() - tail_start);
    let text = format!(
        "{}\n\n[... {omitted} bytes omitted ...]\n\n{}",
        &full_output[..head_end],
        &full_output[tail_start..]
    );
    (text, true)
}

fn last_line_bytes(output: &str) -> usize {
    output.rsplit('\n').next().unwrap_or("").len()
}

fn persist_full_output(prefix: &str, content: &str) -> Result<String, String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}.txt", std::process::id()));
    std::fs::write(&path, content.as_bytes())
        .map_err(|error| format!("Failed to persist full output: {error}"))?;
    Ok(path.to_string_lossy().into_owned())
}

fn format_output(
    snapshot: &TruncationResult,
    full_output: &str,
    full_output_path: Option<&str>,
) -> (String, Option<BashDetails>) {
    let mut text = if snapshot.content.is_empty() {
        "(no output)".to_string()
    } else {
        snapshot.content.clone()
    };
    if !snapshot.truncated {
        return (text, None);
    }

    let details = BashDetails {
        truncation: Some(snapshot.clone()),
        full_output_path: full_output_path.map(str::to_string),
    };
    let start_line = snapshot.total_lines.saturating_sub(snapshot.output_lines) + 1;
    let end_line = snapshot.total_lines;
    let path = full_output_path.unwrap_or("(no output path)");
    if snapshot.last_line_partial {
        text.push_str(&format!(
            "\n\n[Showing last {} of line {start_line} (line is {}). Full output: {path}]",
            format_size(snapshot.output_bytes),
            format_size(last_line_bytes(full_output))
        ));
    } else if snapshot.truncated_by == super::truncate::TruncatedBy::Lines {
        text.push_str(&format!(
            "\n\n[Showing lines {start_line}-{end_line} of {}. Full output: {path}]",
            snapshot.total_lines
        ));
    } else {
        text.push_str(&format!(
            "\n\n[Showing lines {start_line}-{end_line} of {} ({} limit). Full output: {path}]",
            snapshot.total_lines,
            format_size(DEFAULT_MAX_BYTES)
        ));
    }
    (text, Some(details))
}

struct ProcessTreeGuard(Option<u32>);

impl ProcessTreeGuard {
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

impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

struct StreamPipe<R> {
    reader: Option<R>,
    buf: [u8; 8192],
    pending: usize,
}

impl<R: AsyncRead + Unpin> StreamPipe<R> {
    fn new(reader: Option<R>) -> Self {
        Self {
            reader,
            buf: [0; 8192],
            pending: 0,
        }
    }

    async fn read_chunk(&mut self) -> std::io::Result<usize> {
        let Some(reader) = &mut self.reader else {
            return Ok(0);
        };
        reader.read(&mut self.buf[self.pending..]).await
    }

    fn ingest(
        &mut self,
        read: usize,
        combined: &mut Vec<u8>,
        on_chunk: Option<&BashChunkCallback>,
    ) {
        let end = self.pending + read;
        if read == 0 {
            self.reader = None;
            self.pending = 0;
            if end > 0 {
                combined.extend_from_slice(&self.buf[..end]);
                if let Some(cb) = on_chunk {
                    let text = String::from_utf8_lossy(&self.buf[..end]);
                    if !text.is_empty() {
                        cb(&text);
                    }
                }
            }
            return;
        }
        let mut complete = end;
        if let Some(start) = self.buf[..end]
            .iter()
            .rposition(|&byte| byte & 0xc0 != 0x80)
            && end - start < 4
            && std::str::from_utf8(&self.buf[start..end])
                .is_err_and(|error| error.error_len().is_none())
        {
            complete = start;
        }
        if complete > 0 {
            combined.extend_from_slice(&self.buf[..complete]);
            if let Some(cb) = on_chunk {
                let text = String::from_utf8_lossy(&self.buf[..complete]);
                if !text.is_empty() {
                    cb(&text);
                }
            }
        }
        self.buf.copy_within(complete..end, 0);
        self.pending = end - complete;
    }
}

enum ShellWaitOutcome {
    Status(std::io::Result<ExitStatus>),
    TimedOut,
    Aborted,
}

/// Run a command through `shell args... command`, capturing both output streams.
pub async fn execute_shell(
    input: &BashInput,
    cwd: &Path,
    shell: &str,
    args: &[&str],
    shell_name: &str,
    temp_file_prefix: &str,
) -> Result<BashResult, String> {
    execute_shell_with(
        input,
        cwd,
        shell,
        args,
        shell_name,
        temp_file_prefix,
        None,
        None,
    )
    .await
}

/// Run a command with incremental chunk streaming and mid-flight cancellation / process-group kill.
#[expect(
    clippy::too_many_arguments,
    reason = "Internal shared shell executor with optional signal and chunk streaming"
)]
pub async fn execute_shell_with(
    input: &BashInput,
    cwd: &Path,
    shell: &str,
    args: &[&str],
    shell_name: &str,
    temp_file_prefix: &str,
    signal: Option<&CancellationToken>,
    on_chunk: Option<BashChunkCallback>,
) -> Result<BashResult, String> {
    let timeout_ms = resolve_timeout_ms(input.timeout)?;
    if !cwd.exists() {
        return Err(format!(
            "Working directory does not exist: {}\nCannot execute {shell_name} commands.",
            cwd.display()
        ));
    }
    if signal.is_some_and(CancellationToken::is_cancelled) {
        return Err("Command aborted".to_owned());
    }

    let started_at = Instant::now();
    let mut command = Command::new(shell);
    command
        .args(args)
        .arg(&input.command)
        .current_dir(cwd)
        .envs(NON_INTERACTIVE_ENV.iter().copied());
    if std::env::var_os("PI_CODING_AGENT").is_none() {
        command.env("PI_CODING_AGENT", "true");
    }
    if std::env::var_os("AI_AGENT").is_none() {
        command.env("AI_AGENT", crate::config::APP_NAME);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("Failed to spawn {shell_name}: {error}"))?;
    let mut tree = ProcessTreeGuard(child.id());

    let mut stdout_pipe = StreamPipe::new(child.stdout.take());
    let mut stderr_pipe = StreamPipe::new(child.stderr.take());
    let mut combined = Vec::new();

    let cancel_future = async {
        match signal {
            Some(token) => {
                token.cancelled().await;
            }
            None => std::future::pending::<()>().await,
        }
    };
    let timeout_future = async {
        match timeout_ms {
            Some(millis) => {
                tokio::time::sleep(Duration::from_millis(millis)).await;
            }
            None => std::future::pending::<()>().await,
        }
    };

    let outcome = {
        let pump = async {
            while stdout_pipe.reader.is_some() || stderr_pipe.reader.is_some() {
                tokio::select! {
                    read = stdout_pipe.read_chunk(), if stdout_pipe.reader.is_some() => {
                        let read = read.unwrap_or(0);
                        stdout_pipe.ingest(read, &mut combined, on_chunk.as_ref());
                    }
                    read = stderr_pipe.read_chunk(), if stderr_pipe.reader.is_some() => {
                        let read = read.unwrap_or(0);
                        stderr_pipe.ingest(read, &mut combined, on_chunk.as_ref());
                    }
                }
            }
        };
        let wait = async {
            tokio::select! {
                result = child.wait() => ShellWaitOutcome::Status(result),
                () = timeout_future => {
                    tree.kill();
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    ShellWaitOutcome::TimedOut
                }
                () = cancel_future => {
                    tree.kill();
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    ShellWaitOutcome::Aborted
                }
            }
        };
        tokio::pin!(pump, wait);
        tokio::select! {
            outcome = &mut wait => {
                let _ = timeout(Duration::from_millis(EXIT_STDIO_GRACE_MS), &mut pump).await;
                outcome
            }
            () = &mut pump => wait.await,
        }
    };

    if matches!(&outcome, ShellWaitOutcome::Status(_)) {
        tree.0 = None;
    }

    let elapsed_ms = started_at.elapsed().as_millis() as f64;
    let wall_time_seconds = (elapsed_ms / 100.0).round() / 10.0;
    let full_output = String::from_utf8_lossy(&combined).into_owned();

    let snapshot = truncate_tail(&full_output, &TruncationOptions::default());
    let full_output_path = if snapshot.truncated {
        persist_full_output(temp_file_prefix, &full_output).ok()
    } else {
        None
    };
    let (text, details) = format_output(&snapshot, &full_output, full_output_path.as_deref());

    match outcome {
        ShellWaitOutcome::Aborted => Err(append_status(&text, "Command aborted")),
        ShellWaitOutcome::TimedOut => {
            let seconds = input
                .timeout
                .map_or_else(|| "undefined".to_string(), |value| value.to_string());
            Ok(BashResult {
                text: append_status(&text, &format!("Command timed out after {seconds} seconds")),
                details,
                structured: BashOutput {
                    output: full_output,
                    truncated: snapshot.truncated,
                    full_output_path,
                    exit_code: -1,
                    wall_time_seconds,
                },
                is_error: true,
            })
        }
        ShellWaitOutcome::Status(wait_result) => {
            let status =
                wait_result.map_err(|error| format!("Failed to wait for {shell_name}: {error}"))?;
            let exit_code = status.code().unwrap_or(1);
            let (structured_output_text, structured_truncated) = structured_output(&full_output);
            let structured = BashOutput {
                output: structured_output_text,
                truncated: structured_truncated || snapshot.truncated,
                full_output_path: full_output_path.clone(),
                exit_code,
                wall_time_seconds,
            };

            if exit_code != 0 {
                return Ok(BashResult {
                    text: append_status(&text, &format!("Command exited with code {exit_code}")),
                    details,
                    structured,
                    is_error: true,
                });
            }

            Ok(BashResult {
                text,
                details,
                structured,
                is_error: false,
            })
        }
    }
}

fn append_status(text: &str, status: &str) -> String {
    if text.is_empty() {
        status.to_string()
    } else {
        format!("{text}\n\n{status}")
    }
}

/// Bash tool: `/bin/bash -c <command>` on Unix, falling back to `bash`/`sh`.
pub async fn execute_bash(input: &BashInput, cwd: &Path) -> Result<BashResult, String> {
    execute_bash_with(input, cwd, None, None).await
}

pub async fn execute_bash_with(
    input: &BashInput,
    cwd: &Path,
    signal: Option<&CancellationToken>,
    on_chunk: Option<BashChunkCallback>,
) -> Result<BashResult, String> {
    #[cfg(windows)]
    let (shell, args) = ("bash".to_string(), vec!["-c"]);
    #[cfg(not(windows))]
    let (shell, args) = if Path::new("/bin/bash").exists() {
        ("/bin/bash".to_string(), vec!["-c"])
    } else {
        ("sh".to_string(), vec!["-c"])
    };
    execute_shell_with(
        input, cwd, &shell, &args, "bash", "pi-bash", signal, on_chunk,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timeout_error(result: Result<Option<u64>, String>) -> String {
        match result {
            Ok(_) => "<unexpected success>".to_string(),
            Err(error) => error,
        }
    }

    #[test]
    fn resolve_timeout_validation() -> Result<(), String> {
        assert_eq!(resolve_timeout_ms(None)?, None);
        assert_eq!(resolve_timeout_ms(Some(0.5))?, Some(500));
        assert_eq!(
            timeout_error(resolve_timeout_ms(Some(0.0))),
            "Invalid timeout: must be a finite number of seconds"
        );
        assert_eq!(
            timeout_error(resolve_timeout_ms(Some(f64::NAN))),
            "Invalid timeout: must be a finite number of seconds"
        );
        assert_eq!(
            timeout_error(resolve_timeout_ms(Some(3_000_000.0))),
            "Invalid timeout: maximum is 2147483.647 seconds"
        );
        Ok(())
    }

    #[tokio::test]
    async fn runs_command_and_captures_stdout() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = BashInput {
            command: "echo hello".into(),
            timeout: None,
        };
        let result = match execute_bash(&input, dir.path()).await {
            Ok(result) => result,
            Err(error) => return Err(std::io::Error::other(error)),
        };
        assert_eq!(result.text, "hello\n");
        assert_eq!(result.structured.exit_code, 0);
        assert!(!result.is_error);
        Ok(())
    }

    #[tokio::test]
    async fn nonzero_exit_reports_code() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = BashInput {
            command: "echo out; exit 3".into(),
            timeout: None,
        };
        let result = match execute_bash(&input, dir.path()).await {
            Ok(result) => result,
            Err(error) => return Err(std::io::Error::other(error)),
        };
        assert!(result.text.ends_with("Command exited with code 3"));
        assert_eq!(result.structured.exit_code, 3);
        assert!(result.is_error);
        Ok(())
    }

    #[tokio::test]
    async fn timeout_reports_message() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = BashInput {
            command: "sleep 5".into(),
            timeout: Some(0.05),
        };
        let result = match execute_bash(&input, dir.path()).await {
            Ok(result) => result,
            Err(error) => return Err(std::io::Error::other(error)),
        };
        assert!(result.text.to_lowercase().contains("timed out"));
        Ok(())
    }

    #[test]
    fn format_output_includes_full_output_path() {
        let snapshot = truncate_tail(
            "a\nb",
            &TruncationOptions {
                max_lines: Some(1),
                max_bytes: None,
            },
        );
        let (text, details) = format_output(&snapshot, "a\nb", Some("/tmp/full.txt"));
        assert!(text.contains("Full output: /tmp/full.txt"));
        assert!(details.is_some());
    }

    #[tokio::test]
    async fn sets_non_interactive_env_and_allows_inline_override() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = BashInput {
            command: r#"printf '%s|%s|%s\n' "$PAGER" "$GIT_TERMINAL_PROMPT" "$EDITOR"; PAGER=custom sh -c 'printf "%s\n" "$PAGER"'"#.into(),
            timeout: None,
        };
        let result = match execute_bash(&input, dir.path()).await {
            Ok(result) => result,
            Err(error) => return Err(std::io::Error::other(error)),
        };
        assert!(!result.is_error, "unexpected error: {}", result.text);
        assert_eq!(result.text, "cat|0|true\ncustom\n");
        Ok(())
    }
}
