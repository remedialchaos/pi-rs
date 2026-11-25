//! Open a URL in the platform browser, ported from `utils/open-browser.ts`.
//!
//! Upstream deliberately never uses a shell: on Windows `cmd /c start` would re-parse
//! metacharacters in an attacker-controlled URL, so `rundll32 url.dll,FileProtocolHandler` is used
//! instead. This port keeps that platform boundary and passes the target as a single argv item.

use std::path::Path;
use std::process::{Command, Stdio};

/// Resolve the launcher without spawning it, for callers that need to inspect or test the command.
pub fn browser_command(target: &str) -> (&'static str, Vec<String>) {
    if cfg!(target_os = "macos") {
        ("open", vec![target.to_string()])
    } else if cfg!(windows) {
        (
            "rundll32",
            vec![
                "url.dll,FileProtocolHandler".to_string(),
                target.to_string(),
            ],
        )
    } else {
        ("xdg-open", vec![target.to_string()])
    }
}

/// Best-effort launch: returns the spawn error instead of letting it crash the caller.
pub fn try_open_browser(target: &str) -> Result<(), String> {
    let (program, args) = browser_command(target);
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(Path::new("/"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // A new process group keeps the launcher independent of our terminal session.
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let child = command
        .spawn()
        .map_err(|error| format!("Failed to launch browser: {error}"))?;
    // Reap in the background so the detached launcher does not become a zombie.
    std::thread::spawn(move || {
        let _ = child.wait_with_output();
    });
    Ok(())
}

/// Fire-and-forget variant matching upstream's `openBrowser` signature.
pub fn open_browser(target: &str) {
    let _ = try_open_browser(target);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_command_uses_platform_launcher() {
        let (program, args) = browser_command("https://example.com/?a=1&b=2");
        if cfg!(target_os = "macos") {
            assert_eq!(program, "open");
        } else if cfg!(windows) {
            assert_eq!(program, "rundll32");
            assert_eq!(args[0], "url.dll,FileProtocolHandler");
        } else {
            assert_eq!(program, "xdg-open");
        }
        assert_eq!(
            args.last().map(String::as_str),
            Some("https://example.com/?a=1&b=2")
        );
    }

    #[test]
    fn target_is_a_single_argv_item() {
        let (_, args) = browser_command("https://example.com/a b");
        assert!(args.iter().any(|arg| arg == "https://example.com/a b"));
    }
}
