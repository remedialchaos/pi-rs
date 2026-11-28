//! TUI host clipboard copy helpers (`apps/p2/src/hosts/tui-clipboard.ts`).
//! Emits OSC 52 for terminal clipboards and spawns platform clipboard tools (e.g., `wl-copy -t text/plain`).

use base64::{Engine, engine::general_purpose::STANDARD};
use std::{
    collections::BTreeMap,
    io::Write,
    process::{Command, Stdio},
};

const OSC52_MAX_BYTES: usize = 75_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardCommand {
    pub program: String,
    pub args: Vec<String>,
}

/// Returns the ordered native clipboard commands to try on `platform` for the given `env`.
pub fn clipboard_copy_commands(
    env: &BTreeMap<String, String>,
    platform: &str,
) -> Vec<ClipboardCommand> {
    let nonempty = |key: &str| env.get(key).is_some_and(|v| !v.is_empty());
    let cmd = |program: &str, args: &[&str]| ClipboardCommand {
        program: program.to_owned(),
        args: args.iter().map(|s| (*s).to_owned()).collect(),
    };
    match platform {
        "macos" => vec![cmd("pbcopy", &[])],
        "windows" => vec![cmd("clip", &[])],
        _ => {
            let mut commands = Vec::new();
            if nonempty("TERMUX_VERSION") {
                commands.push(cmd("termux-clipboard-set", &[]));
            }
            if nonempty("WAYLAND_DISPLAY") {
                // Explicit UTF-8 text/plain avoids wl-copy misclassifying text snippets (e.g., SVG/XML).
                commands.push(cmd("wl-copy", &["-t", "text/plain"]));
            }
            if nonempty("DISPLAY") {
                commands.push(cmd("xclip", &["-selection", "clipboard"]));
                commands.push(cmd("xsel", &["--clipboard", "--input"]));
            }
            commands
        }
    }
}

/// Copies `text` to the terminal via OSC 52 (when within the 75 KB limit) and attempts native
/// OS clipboard commands best-effort.
pub fn copy_to_clipboard(text: &str, write_osc52: impl FnOnce(&str)) {
    if text.is_empty() {
        return;
    }
    if text.len() <= OSC52_MAX_BYTES {
        let seq = format!("\x1b]52;c;{}\x1b\\", STANDARD.encode(text.as_bytes()));
        write_osc52(&seq);
    }
    let env: BTreeMap<String, String> = std::env::vars().collect();
    let platform = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    };
    for command in clipboard_copy_commands(&env, platform) {
        if try_spawn_clipboard(&command, text) {
            break;
        }
    }
}

fn try_spawn_clipboard(command: &ClipboardCommand, text: &str) -> bool {
    let Ok(mut child) = Command::new(&command.program)
        .args(&command.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_wayland_passes_explicit_text_plain_mime_type_to_wl_copy() {
        let env = BTreeMap::from([("WAYLAND_DISPLAY".into(), "wayland-0".into())]);
        let commands = clipboard_copy_commands(&env, "linux");
        assert_eq!(
            commands,
            vec![ClipboardCommand {
                program: "wl-copy".into(),
                args: vec!["-t".into(), "text/plain".into()],
            }]
        );
    }

    #[test]
    fn copies_osc52_within_byte_limit_and_skips_empty() {
        let mut wrote = String::new();
        copy_to_clipboard("", |s| wrote.push_str(s));
        assert!(wrote.is_empty());

        copy_to_clipboard("hello", |s| wrote.push_str(s));
        assert_eq!(wrote, "\x1b]52;c;aGVsbG8=\x1b\\");
    }
}
