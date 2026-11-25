//! Clipboard command runner, ported from `utils/clipboard-command.ts`.
//!
//! `None` means the command failed; an empty byte vector is a successful result. Commands run
//! with cwd `/` (WSL cannot launch Windows executables from a long `\\wsl.localhost` cwd, #11592)
//! and, on POSIX, in their own process group so a daemonized clipboard owner is not suspended by
//! Ctrl+Z sent to pi's group (#11179).

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

/// Kill the child and (on Unix) its process group, so a descendant cannot keep a pipe open.
fn terminate_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        if let Ok(pid) = i32::try_from(child.id()) {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(-pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub const DEFAULT_TIMEOUT_MS: u64 = 3000;
pub const DEFAULT_MAX_BUFFER_BYTES: usize = 50 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct ClipboardCommandOpts {
    /// Bytes sent to stdin. When set, upstream gives the child no stdout pipe.
    pub input: Option<Vec<u8>>,
    pub timeout_ms: Option<u64>,
    pub max_buffer_bytes: Option<usize>,
}

impl ClipboardCommandOpts {
    pub fn with_text(text: &str) -> Self {
        Self {
            input: Some(text.as_bytes().to_vec()),
            ..Self::default()
        }
    }
}

/// Run a clipboard helper. Writer commands receive `input`; reader commands return captured bytes.
pub fn run_clipboard_command(
    command: &str,
    args: &[String],
    opts: ClipboardCommandOpts,
) -> Option<Vec<u8>> {
    let has_input = opts.input.is_some();
    let mut process = Command::new(command);
    process
        .args(args)
        .current_dir("/")
        .stdin(if has_input {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(if has_input {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        process.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        process.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = process.spawn().ok()?;
    if let Some(input) = opts.input
        && let Some(mut stdin) = child.stdin.take()
    {
        // A writer may exit before consuming all input; BrokenPipe is not a failure.
        let _ = stdin.write_all(&input);
    }

    let limit = opts.max_buffer_bytes.unwrap_or(DEFAULT_MAX_BUFFER_BYTES);
    let overflow = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let reader = if has_input {
        None
    } else {
        let mut stdout = child.stdout.take()?;
        let overflow = Arc::clone(&overflow);
        Some(thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 8 * 1024];
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        bytes.extend_from_slice(&chunk[..n]);
                        if bytes.len() > limit {
                            overflow.store(true, Ordering::Release);
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = tx.send(bytes);
        }))
    };

    let timeout = Duration::from_millis(opts.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        if overflow.load(Ordering::Acquire) {
            terminate_child(&mut child);
            return None;
        }
        if Instant::now() >= deadline {
            terminate_child(&mut child);
            return None;
        }
        thread::sleep(Duration::from_millis(5));
    };
    let status = status?;

    let bytes = match reader {
        Some(handle) => {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let wait = remaining.min(Duration::from_millis(1000));
            let received = rx.recv_timeout(wait).ok();
            if received.is_some() {
                let _ = handle.join();
            }
            received?
        }
        None => Vec::new(),
    };
    if overflow.load(Ordering::Acquire) || !status.success() {
        return None;
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn unix_command(name: &'static str) -> Option<&'static str> {
        if cfg!(windows) { None } else { Some(name) }
    }

    #[test]
    fn writer_sends_unicode_stdin() -> Result<(), String> {
        let Some(cat) = unix_command("/bin/cat") else {
            return Ok(());
        };
        if !Path::new(cat).exists() {
            return Ok(());
        }
        let result = run_clipboard_command(
            cat,
            &[],
            ClipboardCommandOpts::with_text("caf\u{e9} \u{65e5}\u{672c}\u{8a9e}"),
        );
        // Writers have stdout ignored upstream; there is nothing to compare, but the call
        // must succeed (exit 0) rather than fail on a BrokenPipe.
        assert!(result.is_some());
        Ok(())
    }

    #[test]
    fn reader_preserves_binary_bytes() -> Result<(), String> {
        let Some(sh) = unix_command("/bin/sh") else {
            return Ok(());
        };
        if !Path::new(sh).exists() {
            return Ok(());
        }
        let args = vec!["-c".to_string(), "printf '\\000\\377\\012'".to_string()];
        assert_eq!(
            run_clipboard_command(sh, &args, ClipboardCommandOpts::default()),
            Some(vec![0, 255, 10])
        );
        Ok(())
    }

    #[test]
    fn empty_output_is_success_but_nonzero_exit_is_failure() -> Result<(), String> {
        let Some(sh) = unix_command("/bin/sh") else {
            return Ok(());
        };
        if !Path::new(sh).exists() {
            return Ok(());
        }
        let ok = vec!["-c".to_string(), "true".to_string()];
        let fail = vec!["-c".to_string(), "exit 1".to_string()];
        assert_eq!(
            run_clipboard_command(sh, &ok, ClipboardCommandOpts::default()),
            Some(Vec::new())
        );
        assert_eq!(
            run_clipboard_command(sh, &fail, ClipboardCommandOpts::default()),
            None
        );
        Ok(())
    }

    #[test]
    fn missing_command_returns_none() {
        assert_eq!(
            run_clipboard_command(
                "pi-rs-not-a-real-clipboard-helper-9182",
                &[],
                ClipboardCommandOpts::default()
            ),
            None
        );
    }

    #[test]
    fn output_over_limit_fails() -> Result<(), String> {
        let Some(sh) = unix_command("/bin/sh") else {
            return Ok(());
        };
        if !Path::new(sh).exists() {
            return Ok(());
        }
        let args = vec!["-c".to_string(), "printf abcdefghij".to_string()];
        let opts = ClipboardCommandOpts {
            max_buffer_bytes: Some(3),
            ..ClipboardCommandOpts::default()
        };
        assert_eq!(run_clipboard_command(sh, &args, opts), None);
        Ok(())
    }

    #[test]
    fn timeout_returns_none_quickly() -> Result<(), String> {
        let Some(sh) = unix_command("/bin/sh") else {
            return Ok(());
        };
        if !Path::new(sh).exists() {
            return Ok(());
        }
        let args = vec!["-c".to_string(), "sleep 5".to_string()];
        let opts = ClipboardCommandOpts {
            timeout_ms: Some(50),
            ..ClipboardCommandOpts::default()
        };
        let started = Instant::now();
        assert_eq!(run_clipboard_command(sh, &args, opts), None);
        assert!(started.elapsed() < Duration::from_secs(2));
        Ok(())
    }
}
