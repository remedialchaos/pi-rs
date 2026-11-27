//! Launcher-side OS actions: `$EDITOR` editing and browser open
//! (ported from `apps/p2/src/hosts/tui-actions.ts`).

use reqwest::Url;
use std::env;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

const MAX_EDITOR_BYTES: usize = 1024 * 1024;

pub fn resolve_editor_argv(configured: Option<&str>) -> Vec<String> {
    let env_editor = env::var("VISUAL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| env::var("EDITOR").ok().filter(|s| !s.trim().is_empty()));
    let chosen = configured
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
        .or(env_editor)
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "notepad".to_string()
            } else {
                "vi".to_string()
            }
        });
    split_args(&chosen)
}

fn split_args(cmd: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for ch in cmd.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => current.push(ch),
            None if ch == '\'' || ch == '"' => quote = Some(ch),
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
            }
            None => current.push(ch),
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// Writes `initial` to a `0600` temp `.md` file, invokes the external editor synchronously,
/// and returns the edited contents (bounded to 1 MiB).
pub fn run_external_editor(initial: &str, argv: &[String]) -> Result<Option<String>, String> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| "No external editor configured".to_string())?;
    if initial.len() > MAX_EDITOR_BYTES {
        return Err("Editor content exceeds 1 MiB limit".to_string());
    }
    let dir = env::temp_dir().join(format!("p2-tui-editor-{}", uuid::Uuid::new_v4()));
    let mut dir_builder = fs::DirBuilder::new();
    dir_builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        dir_builder.mode(0o700);
    }
    dir_builder
        .create(&dir)
        .map_err(|e| format!("Failed to prepare temp dir for editor: {e}"))?;

    let file_path = dir.join("prompt.md");
    let result = (|| -> Result<Option<String>, String> {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts
            .open(&file_path)
            .map_err(|e| format!("Failed to create editor buffer: {e}"))?;
        file.write_all(initial.as_bytes())
            .map_err(|e| format!("Failed to write editor buffer: {e}"))?;
        drop(file);

        let status = Command::new(program)
            .args(args)
            .arg(&file_path)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|e| format!("Failed to launch {program}: {e}"))?;
        if !status.success() {
            return Err(format!("Editor exited with status {status}"));
        }

        let bytes =
            fs::read(&file_path).map_err(|e| format!("Failed to read edited buffer: {e}"))?;
        if bytes.len() > MAX_EDITOR_BYTES {
            return Err("Edited content exceeds 1 MiB limit".to_string());
        }
        let edited =
            String::from_utf8(bytes).map_err(|_| "Edited file is not valid UTF-8".to_string())?;
        Ok((edited != initial).then_some(edited))
    })();

    let _ = fs::remove_file(&file_path);
    let _ = fs::remove_dir(&dir);
    result
}

/// Validates URL scheme & origin policy and opens the visitor's browser.
pub fn open_browser(url: &Url, allowed_origin: Option<&str>) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("Unsupported URL scheme: {}", url.scheme()));
    }
    if let Some(origin) = allowed_origin
        && url.origin().ascii_serialization() != origin
    {
        return Err(format!("URL leaves allowed origin {origin}"));
    }
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(url.as_str());
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url.as_str()]);
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(url.as_str());
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to open browser: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_editor_argv_splits_quoted_args_and_flags() {
        let argv = resolve_editor_argv(Some("code --wait \"--new-window\""));
        assert_eq!(argv, vec!["code", "--wait", "--new-window"]);
        assert!(!resolve_editor_argv(None).is_empty());
    }

    #[test]
    fn open_browser_rejects_non_http_and_mismatched_origins()
    -> Result<(), Box<dyn std::error::Error>> {
        let file_url = Url::parse("file:///etc/passwd")?;
        assert!(open_browser(&file_url, None).is_err());
        let foreign = Url::parse("https://evil.example/steal")?;
        assert!(open_browser(&foreign, Some("http://127.0.0.1:8788")).is_err());
        Ok(())
    }
}
