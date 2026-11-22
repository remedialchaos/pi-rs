//! Shell discovery and command construction, ported from `utils/shell.ts`.

use std::env;
use std::path::{Path, PathBuf};

/// How a command string reaches the shell. Upstream uses `stdin` for legacy WSL launchers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CommandTransport {
    /// Pass the command as the final argv element (`bash -c '...'`).
    #[default]
    Argv,
    /// Write the command to the shell's stdin (`bash -s`).
    Stdin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellConfig {
    pub shell: String,
    pub args: Vec<String>,
    pub command_transport: CommandTransport,
}

impl ShellConfig {
    pub fn new(shell: impl Into<String>, args: &[&str]) -> Self {
        Self {
            shell: shell.into(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            command_transport: CommandTransport::Argv,
        }
    }
}

fn bash_config(path: String) -> ShellConfig {
    let legacy = is_legacy_wsl_bash_path(&path, env::var("SystemRoot").ok().as_deref());
    let mut config = ShellConfig::new(path, if legacy { &["-s"] } else { &["-c"] });
    if legacy {
        config.command_transport = CommandTransport::Stdin;
    }
    config
}

/// Windows Store aliases are real executables for `access(F_OK)`, so `Path::exists` is the right
/// check here; an alias that resolves to nothing is rejected.
fn executable_exists(path: &str) -> bool {
    Path::new(path).exists()
}

/// Scan PATH without spawning `which`/`where`, so discovery is deterministic and offline.
pub fn find_executable_on_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let with_exe = dir.join(format!("{name}.exe"));
            if with_exe.is_file() {
                return Some(with_exe);
            }
        }
    }
    None
}

/// Resolve the shell, preferring an explicit path, then the environment, then platform defaults.
pub fn get_shell_config(custom_shell_path: Option<&str>) -> Result<ShellConfig, String> {
    if let Some(path) = custom_shell_path {
        if !executable_exists(path) {
            return Err(format!("Custom shell path not found: {path}"));
        }
        return Ok(bash_config(path.to_string()));
    }

    #[cfg(unix)]
    {
        if let Ok(shell) = env::var("SHELL")
            && !shell.is_empty()
            && executable_exists(&shell)
        {
            return Ok(bash_config(shell));
        }
        for candidate in ["/bin/bash", "/usr/bin/bash"] {
            if executable_exists(candidate) {
                return Ok(bash_config(candidate.to_string()));
            }
        }
        if let Some(path) = find_executable_on_path("bash") {
            return Ok(bash_config(path.to_string_lossy().into_owned()));
        }
        if let Some(path) = find_executable_on_path("sh") {
            return Ok(ShellConfig::new(
                path.to_string_lossy().into_owned(),
                &["-c"],
            ));
        }
        // Last resort mirrors upstream's `{ shell: "sh", args: ["-c"] }`.
        Ok(ShellConfig::new("sh", &["-c"]))
    }

    #[cfg(windows)]
    {
        let mut searched = Vec::new();
        for key in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Ok(root) = env::var(key) {
                let candidate = Path::new(&root).join("Git").join("bin").join("bash.exe");
                searched.push(candidate.to_string_lossy().into_owned());
                if candidate.exists() {
                    return Ok(bash_config(candidate.to_string_lossy().into_owned()));
                }
            }
        }
        if let Some(path) = find_executable_on_path("bash.exe") {
            return Ok(bash_config(path.to_string_lossy().into_owned()));
        }
        // The user asked for COMSPEC discovery; fall back to cmd.exe only if it is absent.
        let comspec = env::var("COMSPEC")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "cmd.exe".to_string());
        if executable_exists(&comspec) || find_executable_on_path("cmd.exe").is_some() {
            return Ok(ShellConfig::new(comspec, &["/d", "/s", "/c"]));
        }
        let mut message = String::from(
            "No bash shell found. Options:\n  1. Install Git for Windows: https://git-scm.com/download/win\n  2. Add your bash to PATH (Cygwin, MSYS2, etc.)\n  3. Set shellPath in settings.json\n\nSearched Git Bash in:",
        );
        for path in searched {
            message.push_str("\n  ");
            message.push_str(&path);
        }
        Err(message)
    }

    #[cfg(not(any(unix, windows)))]
    {
        Ok(ShellConfig::new("sh", &["-c"]))
    }
}

/// Build `(argv, stdin_command)` for a shell command. `argv` is ready for `Command::args`.
pub fn shell_command_prefix(config: &ShellConfig, command: &str) -> (Vec<String>, Option<String>) {
    let mut argv = vec![config.shell.clone()];
    argv.extend(config.args.iter().cloned());
    match config.command_transport {
        CommandTransport::Argv => {
            argv.push(command.to_string());
            (argv, None)
        }
        CommandTransport::Stdin => (argv, Some(command.to_string())),
    }
}

/// Quote one argument for a POSIX shell. Safe characters are passed through unchanged.
pub fn escape_sh_arg(value: &str) -> String {
    let safe = |byte: u8| byte.is_ascii_alphanumeric() || b"-_./:=@".contains(&byte);
    if !value.is_empty() && value.bytes().all(safe) {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

/// Quote one argument for `cmd.exe` without invoking a shell in this crate's launchers.
pub fn escape_windows_arg(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./:=@".contains(&b))
    {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('"', "\\\""))
    }
}

/// Expand only a leading `~` or `~/` (and `~\` on Windows), like upstream `normalizePath`.
pub fn expand_tilde_with(path: &str, home: Option<&str>) -> String {
    let separator = if cfg!(windows) { '\\' } else { '/' };
    let rest = if path == "~" {
        Some("")
    } else if let Some(tail) = path.strip_prefix("~/") {
        Some(tail)
    } else if cfg!(windows) {
        path.strip_prefix("~\\")
    } else {
        None
    };
    let Some(rest) = rest else {
        return path.to_string();
    };
    let Some(home) = home else {
        return path.to_string();
    };
    if rest.is_empty() {
        home.to_string()
    } else {
        format!("{home}{separator}{rest}")
    }
}

pub fn expand_tilde(path: &str) -> String {
    let home = env::var("HOME")
        .ok()
        .or_else(|| env::var("USERPROFILE").ok());
    expand_tilde_with(path, home.as_deref())
}

/// Remove control characters that break display width, keeping tab, LF and CR.
pub fn sanitize_binary_output(value: &str) -> String {
    value
        .chars()
        .filter(|c| {
            let code = *c as u32;
            !((0x00..=0x08).contains(&code)
                || code == 0x0b
                || code == 0x0c
                || (0x0e..=0x1f).contains(&code)
                || (0xfff9..=0xfffb).contains(&code))
        })
        .collect()
}

/// Match the legacy `/mnt/c/Windows/System32/bash.exe` launcher that needs `-s` + stdin.
pub fn is_legacy_wsl_bash_path(path: &str, system_root: Option<&str>) -> bool {
    // Upstream applies normalizePath first, which maps `/mnt/c/...` to `C:\...` on Windows.
    let path = super::wsl::wsl_to_windows_path(path).unwrap_or_else(|| path.to_string());
    let normalize = |value: &str| -> String {
        let mut out = value.trim().to_lowercase();
        if out.len() >= 2 && out.as_bytes()[1] == b':' {
            out = format!("{}:{}", &out[..1], out[2..].replace('/', "\\"));
        }
        out
    };
    let path = normalize(&path);
    let is_windows_path = path.len() >= 3
        && path.as_bytes()[0].is_ascii_lowercase()
        && path.as_bytes()[1] == b':'
        && path.as_bytes()[2] == b'\\';
    if !is_windows_path {
        return false;
    }

    if let Some(root) = system_root {
        let root = normalize(root);
        let drive_absolute = root.len() >= 3
            && root.as_bytes()[0].is_ascii_lowercase()
            && root.as_bytes()[1] == b':'
            && (root.as_bytes()[2] == b'\\' || root.as_bytes()[2] == b'/');
        if drive_absolute {
            let base = root.trim_end_matches('\\');
            return path == format!("{base}\\system32\\bash.exe")
                || path == format!("{base}\\sysnative\\bash.exe");
        }
    }

    let prefix = path.strip_suffix("\\bash.exe").unwrap_or(&path);
    prefix == "c:\\windows\\system32"
        || prefix == "c:\\windows\\sysnative"
        || (prefix.len() > 3
            && prefix.as_bytes()[0].is_ascii_lowercase()
            && prefix.as_bytes()[1] == b':'
            && (prefix.ends_with("\\windows\\system32")
                || prefix.ends_with("\\windows\\sysnative")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_uses_argv_for_c_and_stdin_for_s() -> Result<(), String> {
        let c = ShellConfig::new("/bin/bash", &["-c"]);
        let (argv, stdin) = shell_command_prefix(&c, "echo hi");
        assert_eq!(argv, vec!["/bin/bash", "-c", "echo hi"]);
        assert_eq!(stdin, None);
        let mut s = ShellConfig::new("/bin/bash", &["-s"]);
        s.command_transport = CommandTransport::Stdin;
        let (argv, stdin) = shell_command_prefix(&s, "echo hi");
        assert_eq!(argv, vec!["/bin/bash", "-s"]);
        assert_eq!(stdin.as_deref(), Some("echo hi"));
        Ok(())
    }

    #[test]
    fn escape_sh_arg_quotes_spaces_and_single_quotes() {
        assert_eq!(escape_sh_arg("plain-path"), "plain-path");
        assert_eq!(escape_sh_arg("a b"), "'a b'");
        assert_eq!(escape_sh_arg("it's"), "'it'\\''s'");
        assert_eq!(escape_sh_arg(""), "''");
    }

    #[test]
    fn escape_windows_arg_quotes_metacharacters() {
        assert_eq!(escape_windows_arg("plain"), "plain");
        assert_eq!(escape_windows_arg("a&b"), "\"a&b\"");
        assert_eq!(escape_windows_arg("say \"hi\""), "\"say \\\"hi\\\"\"");
    }

    #[test]
    fn expand_tilde_only_at_start() {
        assert_eq!(expand_tilde_with("~", Some("/home/pi")), "/home/pi");
        assert_eq!(expand_tilde_with("~/src", Some("/home/pi")), "/home/pi/src");
        assert_eq!(expand_tilde_with("a/~/b", Some("/home/pi")), "a/~/b");
        assert_eq!(expand_tilde_with("~other/x", Some("/home/pi")), "~other/x");
        assert_eq!(expand_tilde_with("~/x", None), "~/x");
    }

    #[test]
    fn sanitize_binary_output_keeps_tab_lf_cr_and_unicode() {
        assert_eq!(
            sanitize_binary_output("a\u{0000}b\u{0009}c\u{000a}d\u{000d}e"),
            "ab\tc\nd\re"
        );
        assert_eq!(sanitize_binary_output("\u{fff9}x\u{fffb}"), "x");
        assert_eq!(sanitize_binary_output("日本語"), "日本語");
    }

    #[test]
    fn legacy_wsl_detection_matrix() {
        assert!(is_legacy_wsl_bash_path(
            "/mnt/c/Windows/System32/bash.exe",
            Some("C:\\Windows")
        ));
        assert!(is_legacy_wsl_bash_path(
            "c:\\windows\\sysnative\\bash.exe",
            None
        ));
        assert!(is_legacy_wsl_bash_path(
            "D:\\WinNT\\Sysnative\\bash.exe",
            Some("D:\\WinNT")
        ));
        assert!(!is_legacy_wsl_bash_path(
            "C:\\Program Files\\Git\\bin\\bash.exe",
            None
        ));
        assert!(!is_legacy_wsl_bash_path("bash.exe", None));
    }

    #[test]
    fn custom_missing_shell_has_exact_error() {
        let error = get_shell_config(Some("/pi-rs/no/such/shell-9182"));
        assert_eq!(
            error.err().as_deref(),
            Some("Custom shell path not found: /pi-rs/no/such/shell-9182")
        );
    }

    #[test]
    fn get_shell_config_returns_a_usable_shell() -> Result<(), String> {
        let config = get_shell_config(None)?;
        assert!(!config.shell.is_empty());
        assert!(!config.args.is_empty());
        Ok(())
    }
}
