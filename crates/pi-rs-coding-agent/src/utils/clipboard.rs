//! Text clipboard helpers, ported from `utils/clipboard.ts`.
//!
//! Rust has no built-in native clipboard backend in this crate, so platform command resolution is
//! the primary path; OSC 52 is emitted for remote/headless sessions exactly as upstream does.
//! Every command is executed through `std::process::Command` (via `clipboard_command`).

use std::io::Write;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use super::clipboard_command::{ClipboardCommandOpts, run_clipboard_command};
use super::output_files::create_output_file_stream;
use super::wsl::is_wsl_with;

pub const MAX_OSC52_ENCODED_LENGTH: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Linux,
    Darwin,
    Windows,
    Android,
    Other,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Darwin
        } else if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "android") {
            Self::Android
        } else {
            Self::Linux
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClipboardEnv {
    pub wayland_display: Option<String>,
    pub display: Option<String>,
    pub termux_version: Option<String>,
    pub ssh_connection: Option<String>,
    pub ssh_client: Option<String>,
    pub mosh_connection: Option<String>,
    pub pi_clipboard_osc52: Option<String>,
    pub wt_session: Option<String>,
    pub xdg_session_type: Option<String>,
}

impl ClipboardEnv {
    pub fn from_env() -> Self {
        let get = |key: &str| std::env::var(key).ok();
        Self {
            wayland_display: get("WAYLAND_DISPLAY"),
            display: get("DISPLAY"),
            termux_version: get("TERMUX_VERSION"),
            ssh_connection: get("SSH_CONNECTION"),
            ssh_client: get("SSH_CLIENT"),
            mosh_connection: get("MOSH_CONNECTION"),
            pi_clipboard_osc52: get("PI_CLIPBOARD_OSC52"),
            wt_session: get("WT_SESSION"),
            xdg_session_type: get("XDG_SESSION_TYPE"),
        }
    }

    pub fn is_remote_session(&self) -> bool {
        self.ssh_connection.is_some()
            || self.ssh_client.is_some()
            || self.mosh_connection.is_some()
            || self.pi_clipboard_osc52.as_deref() == Some("1")
    }

    pub fn is_headless_linux(&self, platform: Platform) -> bool {
        platform == Platform::Linux
            && self.display.is_none()
            && self.wayland_display.is_none()
            && self.termux_version.is_none()
    }

    fn is_wsl(&self) -> bool {
        is_wsl_with(|key| match key {
            "WSL_DISTRO_NAME" => std::env::var(key).ok(),
            "WSLENV" => std::env::var(key).ok(),
            _ => None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardCommandSpec {
    pub program: &'static str,
    pub args: Vec<String>,
}

pub(crate) fn command(program: &'static str, args: &[&str]) -> ClipboardCommandSpec {
    ClipboardCommandSpec {
        program,
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
    }
}

/// Upstream read order: Termux, Wayland, then X11. An empty Wayland result must not fall through
/// to a stale X11 clipboard (#7248).
pub fn read_commands(platform: Platform, env: &ClipboardEnv) -> Vec<ClipboardCommandSpec> {
    let mut commands = Vec::new();
    if env.termux_version.is_some() {
        commands.push(command("termux-clipboard-get", &[]));
    }
    if platform == Platform::Linux {
        if env.wayland_display.is_some() {
            commands.push(command("wl-paste", &["--no-newline", "--type", "text"]));
        }
        if env.display.is_some() {
            commands.push(command("xclip", &["-selection", "clipboard", "-out"]));
            commands.push(command("xsel", &["--clipboard", "--output"]));
        }
    }
    commands
}

pub fn write_commands(platform: Platform, env: &ClipboardEnv) -> Vec<ClipboardCommandSpec> {
    let mut commands = Vec::new();
    match platform {
        Platform::Darwin => commands.push(command("pbcopy", &[])),
        Platform::Windows => commands.push(command("clip", &[])),
        _ => {
            if env.termux_version.is_some() {
                commands.push(command("termux-clipboard-set", &[]));
            }
            if env.wayland_display.is_some() {
                commands.push(command("wl-copy", &["-t", "text/plain"]));
            }
            if env.display.is_some() {
                commands.push(command("xclip", &["-selection", "clipboard"]));
                commands.push(command("xsel", &["--clipboard", "--input"]));
            }
        }
    }
    commands
}

/// OSC 52 encoding. A payload over 100,000 base64 characters cannot be reliably pasted.
pub fn encode_osc52(text: &str) -> Result<String, String> {
    let encoded = STANDARD.encode(text.as_bytes());
    if encoded.len() > MAX_OSC52_ENCODED_LENGTH {
        return Err("Clipboard unavailable: text exceeds the OSC 52 size limit".to_string());
    }
    Ok(format!("\x1b]52;c;{encoded}\x07"))
}

pub fn emit_osc52(text: &str) -> Result<(), String> {
    let sequence = encode_osc52(text)?;
    let mut stdout = std::io::stdout();
    stdout
        .write_all(sequence.as_bytes())
        .map_err(|error| format!("Clipboard unavailable: {error}"))?;
    stdout
        .flush()
        .map_err(|error| format!("Clipboard unavailable: {error}"))
}

/// WSL without WSLg has no Linux clipboard. Write UTF-8 to a private temp file and let PowerShell
/// read it, because piping bytes mangles non-ASCII text with the console code page.
fn copy_via_windows_clipboard(text: &str) -> bool {
    let (path, mut file) = match create_output_file_stream("pi-wsl-clip", ".txt") {
        Ok(result) => result,
        Err(_) => return false,
    };
    if file.write_all(text.as_bytes()).is_err() {
        let _ = std::fs::remove_file(&path);
        return false;
    }
    drop(file);
    let path_string = path.to_string_lossy().into_owned();
    let win_path = run_clipboard_command(
        "wslpath",
        &["-w".to_string(), path_string.clone()],
        ClipboardCommandOpts {
            timeout_ms: Some(1000),
            ..ClipboardCommandOpts::default()
        },
    )
    .and_then(|bytes| String::from_utf8(bytes).ok())
    .map(|value| value.trim().to_string())
    .filter(|value| !value.is_empty());
    let Some(win_path) = win_path else {
        let _ = std::fs::remove_file(&path);
        return false;
    };
    let script = format!(
        "Set-Clipboard -Value ([System.IO.File]::ReadAllText('{}', [System.Text.Encoding]::UTF8))",
        win_path.replace('\'', "''")
    );
    let result = run_clipboard_command(
        "powershell.exe",
        &["-NoProfile".to_string(), "-Command".to_string(), script],
        ClipboardCommandOpts {
            timeout_ms: Some(5000),
            ..ClipboardCommandOpts::default()
        },
    );
    let _ = std::fs::remove_file(&path);
    result.is_some()
}

/// Pick the exact upstream failure message for a platform/environment combination.
pub fn clipboard_error(platform: Platform, env: &ClipboardEnv, oversized: bool) -> String {
    if oversized {
        return "Clipboard unavailable: text exceeds the OSC 52 size limit".to_string();
    }
    if env.termux_version.is_some() {
        return "Clipboard unavailable: install the Termux:API app and `termux-api` package"
            .to_string();
    }
    if platform == Platform::Linux {
        if env.wayland_display.is_some() {
            return "Clipboard unavailable: install `wl-clipboard` (`wl-copy`) or check Wayland access".to_string();
        }
        if env.display.is_some() {
            return "Clipboard unavailable: install `xclip` or `xsel`, or check X11 access"
                .to_string();
        }
    }
    "Clipboard unavailable".to_string()
}

/// Copy plain text to the clipboard. Errors mirror upstream's exact messages.
pub fn copy_text(text: &str, platform: Platform, env: &ClipboardEnv) -> Result<(), String> {
    let opts = ClipboardCommandOpts::with_text(text);
    let mut copied = false;
    for item in write_commands(platform, env) {
        let command_opts = ClipboardCommandOpts {
            timeout_ms: Some(5000),
            ..opts.clone()
        };
        if run_clipboard_command(item.program, &item.args, command_opts).is_some() {
            copied = true;
            break;
        }
    }

    let wsl = platform == Platform::Linux && env.is_wsl();
    let mut osc52_emitted = false;
    if !copied && wsl {
        if env.wt_session.is_some() {
            osc52_emitted = emit_osc52(text).is_ok();
        }
        copied = osc52_emitted || copy_via_windows_clipboard(text);
    }

    let headless = env.is_headless_linux(platform);
    let mut oversized = false;
    if !osc52_emitted && (env.is_remote_session() || (!copied && headless)) {
        match emit_osc52(text) {
            Ok(()) => copied = true,
            Err(error) => {
                if error == "Clipboard unavailable: text exceeds the OSC 52 size limit" {
                    oversized = true;
                }
            }
        }
    }
    if copied {
        return Ok(());
    }
    Err(clipboard_error(platform, env, oversized))
}

/// Read plain text. An empty clipboard is represented as an empty string, since the Rust API has
/// no `null` in its `Result<String, String>` signature.
pub fn read_text(platform: Platform, env: &ClipboardEnv) -> Result<String, String> {
    for item in read_commands(platform, env) {
        let opts = ClipboardCommandOpts {
            timeout_ms: Some(5000),
            ..ClipboardCommandOpts::default()
        };
        if let Some(bytes) = run_clipboard_command(item.program, &item.args, opts) {
            return Ok(String::from_utf8(bytes)
                .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned()));
        }
    }
    Err("Clipboard unavailable".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> ClipboardEnv {
        ClipboardEnv::default()
    }

    #[test]
    fn wayland_read_command_has_exact_args() {
        let env = ClipboardEnv {
            wayland_display: Some("wayland-0".to_string()),
            ..env()
        };
        let commands = read_commands(Platform::Linux, &env);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].program, "wl-paste");
        assert_eq!(commands[0].args, vec!["--no-newline", "--type", "text"]);
    }

    #[test]
    fn x11_read_falls_back_from_xclip_to_xsel() {
        let env = ClipboardEnv {
            display: Some(":0".to_string()),
            ..env()
        };
        let commands = read_commands(Platform::Linux, &env);
        assert_eq!(
            commands.iter().map(|c| c.program).collect::<Vec<_>>(),
            vec!["xclip", "xsel"]
        );
        assert_eq!(commands[0].args, vec!["-selection", "clipboard", "-out"]);
        assert_eq!(commands[1].args, vec!["--clipboard", "--output"]);
    }

    #[test]
    fn termux_read_comes_first() {
        let env = ClipboardEnv {
            termux_version: Some("1".to_string()),
            display: Some(":0".to_string()),
            ..env()
        };
        let commands = read_commands(Platform::Linux, &env);
        assert_eq!(commands[0].program, "termux-clipboard-get");
        assert!(commands[1..].iter().any(|c| c.program == "xclip"));
    }

    #[test]
    fn write_commands_follow_platform_and_display() {
        let wayland = ClipboardEnv {
            wayland_display: Some("wayland-0".to_string()),
            display: Some(":0".to_string()),
            ..env()
        };
        let commands = write_commands(Platform::Linux, &wayland);
        assert_eq!(
            commands.iter().map(|c| c.program).collect::<Vec<_>>(),
            vec!["wl-copy", "xclip", "xsel"]
        );
        assert_eq!(
            write_commands(Platform::Darwin, &env())[0].program,
            "pbcopy"
        );
        assert_eq!(write_commands(Platform::Windows, &env())[0].program, "clip");
    }

    #[test]
    fn osc52_sequence_is_exact() {
        assert_eq!(
            encode_osc52("héllo").as_deref(),
            Ok("\x1b]52;c;aMOpbGxv\x07")
        );
    }

    #[test]
    fn oversized_osc52_uses_exact_error() -> Result<(), String> {
        let too_large = "x".repeat(75_001);
        let error = encode_osc52(&too_large).err().ok_or("expected error")?;
        assert_eq!(
            error,
            "Clipboard unavailable: text exceeds the OSC 52 size limit"
        );
        Ok(())
    }

    #[test]
    fn remote_session_detection() {
        let mut env = env();
        assert!(!env.is_remote_session());
        env.pi_clipboard_osc52 = Some("1".to_string());
        assert!(env.is_remote_session());
        env.pi_clipboard_osc52 = Some("0".to_string());
        env.ssh_client = Some("10.0.0.1".to_string());
        assert!(env.is_remote_session());
    }

    #[test]
    fn headless_linux_detection() {
        assert!(env().is_headless_linux(Platform::Linux));
        assert!(!env().is_headless_linux(Platform::Darwin));
        let env = ClipboardEnv {
            display: Some(":0".to_string()),
            ..env()
        };
        assert!(!env.is_headless_linux(Platform::Linux));
    }

    #[test]
    fn error_message_prefers_wayland_then_x11() {
        let display = ClipboardEnv {
            display: Some(":0".to_string()),
            ..env()
        };
        assert_eq!(
            clipboard_error(Platform::Linux, &display, false),
            "Clipboard unavailable: install `xclip` or `xsel`, or check X11 access"
        );
        let wayland = ClipboardEnv {
            wayland_display: Some("wayland-0".to_string()),
            ..env()
        };
        assert_eq!(
            clipboard_error(Platform::Linux, &wayland, false),
            "Clipboard unavailable: install `wl-clipboard` (`wl-copy`) or check Wayland access"
        );
        assert_eq!(
            clipboard_error(Platform::Linux, &env(), true),
            "Clipboard unavailable: text exceeds the OSC 52 size limit"
        );
    }
}
