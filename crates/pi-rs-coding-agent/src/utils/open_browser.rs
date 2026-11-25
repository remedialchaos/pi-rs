//! Open a URL in the platform browser, ported from `utils/open-browser.ts`.
//!
//! Upstream deliberately never uses a shell: on Windows `cmd /c start` would re-parse
//! metacharacters in an attacker-controlled URL, so `rundll32 url.dll,FileProtocolHandler` is used
//! instead. This port keeps that platform boundary and passes the target as a single argv item.

use std::path::Path;
use std::process::{Command, Stdio};

/// Environment variable overriding the default platform browser launcher.
pub const ENV_BROWSER_CMD: &str = "PI_BROWSER_CMD";

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

/// Resolve the browser launcher command using an explicit `PI_BROWSER_CMD` override if provided,
/// without invoking a shell.
pub fn resolve_browser_command_with(
    target: &str,
    browser_cmd: Option<&str>,
) -> Result<(String, Vec<String>), String> {
    let Some(raw) = browser_cmd.map(str::trim).filter(|cmd| !cmd.is_empty()) else {
        let (program, args) = browser_command(target);
        return Ok((program.to_owned(), args));
    };
    if raw.contains('\0') {
        return Err("Invalid PI_BROWSER_CMD: command contains NUL byte".to_owned());
    }
    if Path::new(raw).is_file() {
        return Ok((raw.to_owned(), vec![target.to_owned()]));
    }
    let tokens = split_command_tokens(raw)?;
    let mut iter = tokens.into_iter();
    let program = iter
        .next()
        .ok_or_else(|| "Invalid PI_BROWSER_CMD: empty command".to_owned())?;
    let mut args: Vec<String> = iter.collect();
    args.push(target.to_owned());
    Ok((program, args))
}

fn split_command_tokens(input: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut quote: Option<char> = None;

    for ch in input.chars() {
        match quote {
            Some(q) if ch == q => {
                quote = None;
            }
            Some(_) => {
                current.push(ch);
            }
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                has_token = true;
            }
            None if ch.is_ascii_whitespace() => {
                if has_token {
                    tokens.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            None => {
                current.push(ch);
                has_token = true;
            }
        }
    }

    if quote.is_some() {
        return Err("Invalid PI_BROWSER_CMD: unclosed quote".to_owned());
    }
    if has_token {
        tokens.push(current);
    }
    if tokens.is_empty() {
        return Err("Invalid PI_BROWSER_CMD: empty command".to_owned());
    }
    Ok(tokens)
}

/// Resolve the browser launcher command using `PI_BROWSER_CMD` if set, otherwise the platform
/// default launcher.
pub fn resolve_browser_command(target: &str) -> Result<(String, Vec<String>), String> {
    let override_cmd = std::env::var(ENV_BROWSER_CMD).ok();
    resolve_browser_command_with(target, override_cmd.as_deref())
}

/// Best-effort launch: returns the spawn error instead of letting it crash the caller.
pub fn try_open_browser(target: &str) -> Result<(), String> {
    let (program, args) = resolve_browser_command(target)?;
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

/// Validate that `target` is an `http:` or `https:` URL and open it in the browser.
pub fn try_open_web_url(target: &str) -> Result<(), String> {
    let normalized = crate::utils::web_url::web_url(target)
        .ok_or_else(|| "Refusing to open non-HTTP(S) URL in browser".to_owned())?;
    try_open_browser(&normalized)
}

/// Fire-and-forget variant of [`try_open_web_url`].
pub fn open_web_url(target: &str) {
    let _ = try_open_web_url(target);
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

    #[test]
    fn resolve_browser_command_uses_pi_browser_cmd_without_shell()
    -> Result<(), Box<dyn std::error::Error>> {
        let (default_prog, default_args) =
            resolve_browser_command_with("https://example.com/auth", None)?;
        let (expected_prog, expected_args) = browser_command("https://example.com/auth");
        assert_eq!(default_prog, expected_prog);
        assert_eq!(default_args, expected_args);

        let (blank_prog, _) =
            resolve_browser_command_with("https://example.com/auth", Some("   "))?;
        assert_eq!(blank_prog, expected_prog);

        let (prog, args) = resolve_browser_command_with(
            "https://example.com/auth?a=1&b=2",
            Some("/usr/bin/chromium --headless=new \"--user-data-dir=/tmp/a b\" '--no-sandbox'"),
        )?;
        assert_eq!(prog, "/usr/bin/chromium");
        assert_eq!(
            args,
            vec![
                "--headless=new",
                "--user-data-dir=/tmp/a b",
                "--no-sandbox",
                "https://example.com/auth?a=1&b=2"
            ]
        );

        let dir = tempfile::tempdir()?;
        let script = dir.path().join("my browser.sh");
        std::fs::write(&script, "#!/bin/sh\n")?;
        let script_str = script.to_string_lossy().into_owned();
        let (file_prog, file_args) =
            resolve_browser_command_with("https://example.com/auth", Some(&script_str))?;
        assert_eq!(file_prog, script_str);
        assert_eq!(file_args, vec!["https://example.com/auth"]);
        Ok(())
    }

    #[test]
    fn resolve_browser_command_rejects_unclosed_quote_and_nul() {
        assert!(
            resolve_browser_command_with("https://example.com", Some("/usr/bin/foo \"unclosed"))
                .is_err()
        );
        assert!(
            resolve_browser_command_with("https://example.com", Some("/usr/bin/foo 'unclosed"))
                .is_err()
        );
        assert!(
            resolve_browser_command_with("https://example.com", Some("/usr/bin/foo\0bar")).is_err()
        );
    }

    #[test]
    fn try_open_web_url_rejects_unsafe_schemes() {
        for unsafe_url in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/html,hi",
            "ftp://example.com/file",
        ] {
            let err = try_open_web_url(unsafe_url)
                .err()
                .unwrap_or_else(|| String::from("expected error"));
            assert!(
                err.contains("non-HTTP(S)"),
                "unexpected error for {unsafe_url}: {err}"
            );
        }
    }
}
