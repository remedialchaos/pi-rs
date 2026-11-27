use super::process::LocalActions;
use crate::{Error, Result};
use base64::Engine;
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

/// Uses the launcher's active raw terminal for confirmation. The subprocess
/// runner pauses sandbox output while an action awaits the operator's answer.
pub struct TerminalActions;

fn confirm(detail: &str, question: &str) -> Result<bool> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(Error::Invalid("confirmation requires a terminal".into()));
    }
    use pi_rs_tui::{
        terminal::{ProcessTerminal, Terminal, TerminalEvent},
        utils::wrap_text_with_ansi,
    };
    use std::time::{Duration, Instant};
    let mut terminal = ProcessTerminal::new();
    let width = terminal.width().saturating_sub(2).max(1);
    let mut lines = wrap_text_with_ansi(detail, width);
    lines.extend(wrap_text_with_ansi(&format!("{question} [y/N]"), width));
    if lines.len() >= terminal.height() {
        return Err(Error::Invalid(
            "confirmation does not fit the terminal".into(),
        ));
    }
    let top = terminal.height() - lines.len() + 1;
    let mut console = std::io::stdout().lock();
    write!(console, "\x1b[?2026l\x1b[s")?;
    for (offset, line) in lines.iter().enumerate() {
        write!(
            console,
            "\x1b[{};1H\x1b[0;30;47m\x1b[2K {line}",
            top + offset
        )?;
    }
    write!(console, "\x1b[0m")?;
    console.flush()?;
    let ready = Instant::now() + Duration::from_millis(300);
    let result = (|| loop {
        for event in terminal.poll_events(Duration::from_millis(100))? {
            match event {
                TerminalEvent::Resize { .. } => return Ok(false),
                TerminalEvent::Input(keys) if keys.contains(char::from(3)) => {
                    return Err(std::io::Error::from(std::io::ErrorKind::Interrupted).into());
                }
                TerminalEvent::Input(keys) if Instant::now() >= ready => {
                    return Ok(matches!(keys.as_str(), "y" | "Y"));
                }
                _ => {}
            }
        }
    })();
    for offset in 0..lines.len() {
        write!(console, "\x1b[{};1H\x1b[0m\x1b[2K", top + offset)?;
    }
    write!(console, "\x1b[u")?;
    console.flush()?;
    result
}

fn hidden(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{ad}' | '\u{61c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
}

impl LocalActions for TerminalActions {
    fn copy(&mut self, text: &str) -> Result<bool> {
        if text.chars().any(|c| hidden(c) && !matches!(c, '\n' | '\t')) {
            return Err(Error::Invalid(
                "clipboard text contains hidden characters".into(),
            ));
        }
        let terminal = pi_rs_tui::terminal::ProcessTerminal::new();
        let width = pi_rs_tui::terminal::Terminal::width(&terminal)
            .saturating_sub(4)
            .max(1);
        let rows = pi_rs_tui::utils::wrap_text_with_ansi(&text.replace('\t', "    "), width);
        let mut shown = rows.iter().take(6).cloned().collect::<Vec<_>>();
        let hidden = rows
            .iter()
            .skip(6)
            .map(|row| row.encode_utf16().count())
            .sum::<usize>();
        if hidden > 0 {
            shown.push(format!("… and {hidden} more characters"));
        }
        let shown = shown.join("\n");
        if !confirm(
            &shown,
            &format!(
                "Copy {} characters to the clipboard?",
                text.encode_utf16().count()
            ),
        )? {
            return Ok(false);
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
        let mut out = std::io::stdout().lock();
        write!(out, "\x1b]52;c;{encoded}\x07")?;
        out.flush()?;
        Ok(true)
    }

    fn open(&mut self, url: &http::Uri) -> Result<bool> {
        let url = url.to_string();
        if !confirm(&url, "Open this URL in the browser?")? {
            return Ok(false);
        }
        #[cfg(target_os = "windows")]
        let mut command = {
            let mut c = std::process::Command::new("rundll32");
            c.arg("url.dll,FileProtocolHandler");
            c
        };
        #[cfg(target_os = "macos")]
        let mut command = std::process::Command::new("open");
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let mut command = std::process::Command::new("xdg-open");
        if !command
            .arg(url)
            .stdin(std::process::Stdio::null())
            .status()?
            .success()
        {
            return Err(Error::Invalid("browser launch failed".into()));
        }
        Ok(true)
    }

    fn attach(&mut self, path: &Path) -> Result<Option<(String, Vec<u8>)>> {
        let invalid = || Error::Invalid("invalid attachment path".into());
        let text = path.to_str().ok_or_else(invalid)?;
        if text.is_empty()
            || text.trim() != text
            || text.contains("  ")
            || text
                .chars()
                .any(|c| hidden(c) || (c.is_whitespace() && c != ' '))
        {
            return Err(invalid());
        }
        #[cfg(windows)]
        {
            // A UNC or device lookup may send credentials before showing a prompt.
            let named = text.replace(char::from(92), "/");
            if named.starts_with("//")
                || named.split('/').any(|part| {
                    let stem = part.split('.').next().unwrap_or("").to_ascii_lowercase();
                    matches!(
                        stem.as_str(),
                        "con" | "prn" | "aux" | "nul" | "conin$" | "conout$"
                    ) || (stem.len() == 4
                        && (stem.starts_with("com") || stem.starts_with("lpt"))
                        && stem.as_bytes()[3].is_ascii_digit())
                })
            {
                return Err(invalid());
            }
        }
        let path = if let Ok(tail) = path.strip_prefix("~") {
            PathBuf::from(
                std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                    .ok_or_else(invalid)?,
            )
            .join(tail)
        } else {
            std::path::absolute(path)?
        };
        #[cfg(windows)]
        {
            if !confirm(path.to_str().ok_or_else(invalid)?, "Attach this file?")? {
                return Ok(None);
            }
            let resolved = path.canonicalize()?;
            let (file, _directories) = super::windows::open_local(&resolved)?;
            let metadata = file.metadata()?;
            if !metadata.is_file() || metadata.len() > 16 * 1024 * 1024 {
                return Err(invalid());
            }
            let shown = resolved
                .to_str()
                .filter(|s| !s.chars().any(hidden))
                .ok_or_else(invalid)?;
            if !confirm(shown, "Attach this resolved file?")? {
                return Ok(None);
            }
            let name = resolved
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(invalid)?
                .to_string();
            let mut bytes = Vec::new();
            file.take(metadata.len()).read_to_end(&mut bytes)?;
            Ok(Some((name, bytes)))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            let _ = path;
            Err(Error::Invalid(
                "attachment identity checking unavailable".into(),
            ))
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            if !confirm(path.to_str().ok_or_else(invalid)?, "Attach this file?")? {
                return Ok(None);
            }
            let resolved = path.canonicalize()?;
            let shown = resolved
                .to_str()
                .filter(|s| !s.chars().any(hidden))
                .ok_or_else(invalid)?;
            let metadata = resolved.metadata()?;
            if !metadata.is_file() || metadata.len() > 16 * 1024 * 1024 {
                return Err(invalid());
            }
            if resolved != path && !confirm(shown, "This path resolves here. Attach this file?")? {
                return Ok(None);
            }
            // O_NONBLOCK | O_NOFOLLOW, from the respective platform headers.
            #[cfg(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "arm")))]
            let flags = 0o4000 | 0o100000;
            #[cfg(all(
                target_os = "linux",
                not(any(target_arch = "aarch64", target_arch = "arm"))
            ))]
            let flags = 0o4000 | 0o400000;
            #[cfg(target_os = "macos")]
            let flags = 0x4 | 0x100;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(flags)
                .open(&resolved)?;
            let opened = file.metadata()?;
            if !opened.is_file()
                || opened.len() > 16 * 1024 * 1024
                || metadata.dev() != opened.dev()
                || metadata.ino() != opened.ino()
            {
                return Err(invalid());
            }
            let mut bytes = Vec::new();
            file.take(opened.len()).read_to_end(&mut bytes)?;
            Ok(Some((
                resolved
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or_else(invalid)?
                    .to_string(),
                bytes,
            )))
        }
    }
}
