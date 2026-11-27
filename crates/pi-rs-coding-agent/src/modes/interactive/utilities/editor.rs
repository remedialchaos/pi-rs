use std::io::{self, Write};
use std::path::{Path, PathBuf};

use tokio::process::Command;
use uuid::Uuid;

use crate::utils::text::strip_bom;

/// Open $VISUAL, $EDITOR, or the platform default. Suspend the TUI before awaiting.
pub async fn external_editor(text: &str) -> Result<Option<String>, String> {
    let command = ["VISUAL", "EDITOR"].into_iter().find_map(|variable| {
        std::env::var(variable)
            .ok()
            .filter(|value| !value.trim().is_empty())
    });
    let default = if cfg!(windows) { "notepad" } else { "nano" };
    external_editor_with_command(command.as_deref().unwrap_or(default), text).await
}

/// Edit a private prompt file in an external process. The child inherits the terminal and cwd.
pub async fn external_editor_with_command(
    command: &str,
    text: &str,
) -> Result<Option<String>, String> {
    let mut words = command.split_ascii_whitespace();
    let Some(program) = words.next() else {
        return Ok(None);
    };
    edit_with(text, |path| {
        let mut child = Command::new(program);
        child.args(words).arg(path).kill_on_drop(true);
        async move {
            let _ = writeln!(
                io::stdout(),
                "Launching external editor: {command}\nPi will resume when the editor exits."
            );
            let status = child
                .status()
                .await
                .map_err(|_| format!("Failed to launch external editor: {command}"))?;
            if !status.success() {
                let reason = status
                    .code()
                    .map_or_else(|| status.to_string(), |code| format!("code {code}"));
                return Err(format!(
                    "External editor exited with {reason}; prompt unchanged"
                ));
            }
            Ok(())
        }
    })
    .await
    .map(Some)
}

async fn edit_with<F, Fut>(text: &str, run: F) -> Result<String, String>
where
    F: FnOnce(&Path) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let directory = PrivateTempDir::new("pi-editor")
        .map_err(|error| format!("External editor failed: {error}; prompt unchanged"))?;
    let file = directory.path().join("prompt.md");
    tokio::fs::write(&file, text)
        .await
        .map_err(|error| format!("External editor failed: {error}; prompt unchanged"))?;
    run(&file).await?;
    let bytes = tokio::fs::read(&file)
        .await
        .map_err(|error| format!("Failed to read editor buffer: {error}"))?;
    let mut edited = String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned());
    if strip_bom(&edited).len() != edited.len() {
        edited.drain(..'\u{feff}'.len_utf8());
    }
    if edited.ends_with('\n') {
        edited.pop();
    }
    Ok(edited)
}

/// `create` is exclusive and the mode is set at creation, not after exposing sensitive files.
pub(super) struct PrivateTempDir(PathBuf);

impl PrivateTempDir {
    pub(super) fn new(prefix: &str) -> io::Result<Self> {
        let path = std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path)?;
        Ok(Self(path))
    }

    pub(super) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PrivateTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn editor_removes_only_bom_and_one_newline() -> Result<(), String> {
        let parent = std::cell::OnceCell::new();
        let saved = &parent;
        let edited = edit_with("initial", |path| {
            let path = path.to_owned();
            async move {
                let input = tokio::fs::read_to_string(&path)
                    .await
                    .map_err(|error| error.to_string())?;
                assert_eq!(input, "initial");
                let directory = path.parent().ok_or("Missing editor directory")?;
                saved
                    .set(directory.to_owned())
                    .map_err(|_| "Duplicate editor invocation")?;
                tokio::fs::write(path, "\u{feff}Résumé\n\n")
                    .await
                    .map_err(|error| error.to_string())
            }
        })
        .await?;
        assert_eq!(edited, "Résumé\n");
        assert!(!parent.get().ok_or("Editor not invoked")?.exists());
        Ok(())
    }

    #[tokio::test]
    async fn editor_failure_does_not_return_modified_prompt() {
        let result = edit_with("original", |_| async { Err("cancelled".into()) }).await;
        assert_eq!(result, Err("cancelled".into()));
    }

    #[tokio::test]
    async fn external_editor_reports_exact_launch_and_exit_errors() {
        let missing = external_editor_with_command("pi-nonexistent-editor-binary-xyz", "hi").await;
        assert_eq!(
            missing,
            Err("Failed to launch external editor: pi-nonexistent-editor-binary-xyz".to_owned())
        );

        let failed = external_editor_with_command("sh -c exit\\ 7", "hi").await;
        assert!(
            matches!(failed, Err(ref msg) if msg.starts_with("External editor exited with code ") && msg.ends_with("; prompt unchanged")),
            "{failed:?}"
        );
    }
}
