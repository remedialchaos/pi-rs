//! Child process helpers ported from `utils/child-process.ts`.
//!
//! `tokio::process::Child::wait()` only reports the exit status; it does not guarantee that all
//! bytes written to inherited stdout/stderr pipes have been read when it returns. Upstream keeps
//! reading for a short idle window after exit (earendil-works/pi#5303): a short-lived child can
//! exit while a detached descendant still owns the pipe, so destroying the pipe on a fixed
//! deadline measured from `exit` truncates output.

use std::process::{Child, Stdio};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command as TokioCommand;
use tokio::sync::watch;

/// Idle window after child exit. The timer is re-armed by every new chunk, so an active
/// descendant keeps the pipe open while a quiet inherited handle is released after 100 ms.
pub const EXIT_STDIO_GRACE_MS: u64 = 100;

/// Drain one pipe until EOF, or until no data arrives for `grace` after the child has exited.
///
/// Before exit there is no deadline: a slow child blocked on a full pipe must not be cut off.
/// `exit_rx` flips once `wait()` has returned, which switches this loop to idle-timer mode.
async fn drain<R>(mut reader: R, mut exit_rx: watch::Receiver<bool>, grace: Duration) -> Vec<u8>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8 * 1024];
    loop {
        let exited = *exit_rx.borrow();
        if exited {
            match tokio::time::timeout(grace, reader.read(&mut chunk)).await {
                Ok(Ok(0)) | Err(_) => break,
                Ok(Ok(n)) => bytes.extend_from_slice(&chunk[..n]),
                Ok(Err(_)) => break,
            }
        } else {
            tokio::select! {
                read = reader.read(&mut chunk) => match read {
                    Ok(0) | Err(_) => break,
                    Ok(n) => bytes.extend_from_slice(&chunk[..n]),
                },
                _ = exit_rx.changed() => {}
            }
        }
    }
    bytes
}

/// Run `program` with `args`, optionally feeding `stdin`, and return `(exit_code, stdout, stderr)`.
///
/// A nonzero exit status is returned as data, not as an error. Spawn, pipe, and wait failures are
/// reported as strings (upstream rejects the promise in the equivalent cases). Signals report
/// exit code `-1`, matching the absence of a numeric code in upstream's `exit` event.
pub async fn run_command(
    program: &str,
    args: &[String],
    stdin: Option<&[u8]>,
) -> Result<(i32, Vec<u8>, Vec<u8>), String> {
    let mut command = TokioCommand::new(program);
    command
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|error| format!("Failed to spawn {program}: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("Failed to capture stdout of {program}"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("Failed to capture stderr of {program}"))?;

    if let Some(input) = stdin {
        let mut handle = child
            .stdin
            .take()
            .ok_or_else(|| format!("Failed to capture stdin of {program}"))?;
        handle
            .write_all(input)
            .await
            .map_err(|error| format!("Failed to write stdin of {program}: {error}"))?;
        // Closing the pipe delivers EOF to the child; keeping it open would hang readers.
        drop(handle);
    }

    let (exit_tx, exit_rx) = watch::channel(false);
    let grace = Duration::from_millis(EXIT_STDIO_GRACE_MS);
    let stdout_task = tokio::spawn(drain(stdout, exit_rx.clone(), grace));
    let stderr_task = tokio::spawn(drain(stderr, exit_rx, grace));

    let status = child
        .wait()
        .await
        .map_err(|error| format!("Failed to wait for {program}: {error}"))?;
    let _ = exit_tx.send(true);

    let stdout = stdout_task
        .await
        .map_err(|error| format!("stdout drain task failed: {error}"))?;
    let stderr = stderr_task
        .await
        .map_err(|error| format!("stderr drain task failed: {error}"))?;
    Ok((status.code().unwrap_or(-1), stdout, stderr))
}

/// Kill a direct child and reap it. An already-finished child is treated as success.
pub fn kill_child_and_wait(child: &mut Child) -> Result<(), String> {
    match child.try_wait() {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {}
        Err(error) => return Err(format!("Failed to check child status: {error}")),
    }
    child
        .kill()
        .map_err(|error| format!("Failed to kill child: {error}"))?;
    child
        .wait()
        .map_err(|error| format!("Failed to reap child: {error}"))?;
    Ok(())
}

/// Best-effort kill of a process and its descendants (upstream `killProcessTree`).
///
/// On Unix the child is expected to be a process-group leader (spawn with `process_group(0)`);
/// a group kill handles its descendants. The group id is the child pid. A failed group kill
/// falls back to signalling the single process. All nix calls used here are safe wrappers.
#[cfg(unix)]
pub fn kill_process_tree(pid: u32) -> Result<(), String> {
    use nix::sys::signal::{Signal, kill, killpg};
    use nix::unistd::Pid;

    let raw = i32::try_from(pid).map_err(|_| format!("Invalid process id: {pid}"))?;
    let group = Pid::from_raw(-raw);
    let process = Pid::from_raw(raw);

    // macOS: stop the group first so a dying child cannot wake the shell before it is killed.
    #[cfg(target_os = "macos")]
    let _ = killpg(group, Signal::SIGSTOP);

    match killpg(group, Signal::SIGKILL) {
        Ok(()) => Ok(()),
        Err(_) => kill(process, Signal::SIGKILL).map_err(|error| error.to_string()),
    }
}

/// Windows counterpart: use the trusted System32 `taskkill.exe` with `/T` for descendants.
#[cfg(windows)]
pub fn kill_process_tree(pid: u32) -> Result<(), String> {
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    let taskkill = std::path::Path::new(&system_root)
        .join("System32")
        .join("taskkill.exe");
    let status = std::process::Command::new(taskkill)
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("taskkill failed with {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn shell() -> Option<&'static str> {
        if cfg!(windows) { None } else { Some("/bin/sh") }
    }

    #[tokio::test]
    async fn run_command_captures_streams_and_nonzero_exit() -> Result<(), String> {
        let Some(sh) = shell() else { return Ok(()) };
        if !Path::new(sh).exists() {
            return Ok(());
        }
        let args = vec![
            "-c".to_string(),
            "printf out; printf err >&2; exit 7".to_string(),
        ];
        let (code, stdout, stderr) = run_command(sh, &args, None).await?;
        assert_eq!(code, 7);
        assert_eq!(stdout, b"out");
        assert_eq!(stderr, b"err");
        Ok(())
    }

    #[tokio::test]
    async fn run_command_forwards_stdin() -> Result<(), String> {
        let Some(cat) = (if cfg!(windows) {
            None
        } else {
            Some("/bin/cat")
        }) else {
            return Ok(());
        };
        if !Path::new(cat).exists() {
            return Ok(());
        }
        let (code, stdout, stderr) = run_command(cat, &[], Some(b"caf\xc3\xa9\n")).await?;
        assert_eq!(code, 0);
        assert_eq!(stdout, b"caf\xc3\xa9\n");
        assert!(stderr.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn run_command_drains_output_larger_than_a_pipe() -> Result<(), String> {
        let Some(sh) = shell() else { return Ok(()) };
        if !Path::new(sh).exists() {
            return Ok(());
        }
        let args = vec![
            "-c".to_string(),
            "dd if=/dev/zero bs=1024 count=256 2>/dev/null".to_string(),
        ];
        let (code, stdout, _) = run_command(sh, &args, None).await?;
        assert_eq!(code, 0);
        assert_eq!(stdout.len(), 256 * 1024);
        Ok(())
    }

    #[tokio::test]
    async fn run_command_reports_spawn_failure() {
        let result = run_command("pi-rs-definitely-not-a-real-binary-9271", &[], None).await;
        assert!(result.is_err());
    }
}
