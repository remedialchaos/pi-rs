//! Content search using `rg --json`, ported from `core/tools/grep.ts`.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use base64::Engine;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};

use super::find::path_relative;
use super::path_utils::resolve_to_cwd;
use super::truncate::{
    DEFAULT_MAX_BYTES, GREP_MAX_LINE_LENGTH, TruncationOptions, TruncationResult, format_size,
    truncate_head, truncate_line,
};

pub const DEFAULT_LIMIT: usize = 100;

#[derive(Clone, Debug, Deserialize)]
pub struct GrepInput {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub glob: Option<String>,
    #[serde(default)]
    pub ignore_case: Option<bool>,
    #[serde(default)]
    pub literal: Option<bool>,
    #[serde(default)]
    pub context: Option<f64>,
    #[serde(default)]
    pub limit: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GrepDetails {
    pub truncation: Option<TruncationResult>,
    pub match_limit_reached: Option<usize>,
    pub lines_truncated: bool,
}

#[derive(Clone, Debug)]
pub struct GrepExecution {
    pub output: String,
    pub details: Option<GrepDetails>,
}

pub trait GrepOperations {
    /// Whether the resolved search target is a directory. Throws in upstream when missing.
    fn is_directory(&self, path: &Path) -> Result<bool, String>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LocalGrepOperations;

impl GrepOperations for LocalGrepOperations {
    fn is_directory(&self, path: &Path) -> Result<bool, String> {
        std::fs::metadata(path)
            .map(|metadata| metadata.is_dir())
            .map_err(|error| error.to_string())
    }
}

pub fn rg_available() -> bool {
    std::process::Command::new("rg")
        .arg("--version")
        .status()
        .is_ok_and(|status| status.success())
}

fn decode_rg_text(data: Option<&Value>) -> String {
    let Some(data) = data else {
        return String::new();
    };
    if let Some(text) = data.get("text").and_then(Value::as_str) {
        return text.to_string();
    }
    if let Some(bytes) = data.get("bytes").and_then(Value::as_str)
        && let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(bytes)
    {
        return String::from_utf8_lossy(&decoded).into_owned();
    }
    String::new()
}

fn format_path(file_path: &str, search_path: &Path, is_directory: bool) -> String {
    if is_directory {
        let candidate = Path::new(file_path);
        let resolved: PathBuf = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            search_path.join(candidate)
        };
        let relative = path_relative(search_path, &resolved);
        let relative_string = relative.to_string_lossy();
        if !relative_string.is_empty() && !relative_string.starts_with("..") {
            return relative_string.replace('\\', "/");
        }
    }
    Path::new(file_path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn sanitize_line_text(text: &str) -> &str {
    if let Some(stripped) = text.strip_suffix('\n') {
        stripped.strip_suffix('\r').unwrap_or(stripped)
    } else {
        text.strip_suffix('\r').unwrap_or(text)
    }
}

async fn kill_child(child: &mut Child) {
    let _ = child.kill().await;
}

/// Execute the grep tool and return its model-facing text plus optional details.
pub async fn execute_grep(
    input: &GrepInput,
    cwd: &Path,
    ops: &dyn GrepOperations,
) -> Result<GrepExecution, String> {
    if !rg_available() {
        return Err("ripgrep (rg) is not available and could not be downloaded".to_string());
    }

    let search_path = resolve_to_cwd(input.path.as_deref().unwrap_or("."), cwd);
    let is_directory = ops
        .is_directory(&search_path)
        .map_err(|_| format!("Path not found: {}", search_path.display()))?;

    let context_value = input
        .context
        .filter(|value| *value > 0.0)
        .map(|value| value.floor().min(9_007_199_254_740_991.0) as usize)
        .unwrap_or(0);
    let effective_limit = input.limit.unwrap_or(DEFAULT_LIMIT as f64).floor().max(1.0) as usize;

    let mut args: Vec<String> = vec![
        "--no-config".into(),
        "--json".into(),
        "--line-number".into(),
        "--color=never".into(),
        "--hidden".into(),
    ];
    if input.ignore_case.unwrap_or(false) {
        args.push("--ignore-case".into());
    }
    if input.literal.unwrap_or(false) {
        args.push("--fixed-strings".into());
    }
    if let Some(glob) = &input.glob {
        let trimmed = glob
            .strip_prefix("./")
            .map(|rest| rest.to_string())
            .unwrap_or_else(|| glob.clone());
        let trimmed = if let Some(rest) = trimmed.strip_prefix("!./") {
            format!("!{rest}")
        } else {
            trimmed
        };
        args.push("--glob".into());
        args.push(trimmed);
    }
    if context_value > 0 {
        args.push("--context".into());
        args.push(context_value.to_string());
    }
    args.push("--".into());
    args.push(input.pattern.clone());
    args.push(if is_directory {
        ".".into()
    } else {
        search_path.to_string_lossy().into_owned()
    });

    let mut command = Command::new("rg");
    command
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if is_directory {
        command.current_dir(&search_path);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("Failed to run ripgrep: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to run ripgrep: missing stdout".to_string())?;
    let stderr_pipe = child.stderr.take();
    let stderr_task = tokio::spawn(async move {
        let mut text = String::new();
        if let Some(pipe) = stderr_pipe {
            let mut reader = BufReader::new(pipe);
            let _ = reader.read_to_string(&mut text).await;
        }
        text
    });

    let mut reader = BufReader::new(stdout).lines();
    let mut output_lines: Vec<String> = Vec::new();
    let mut output_bytes = 0usize;
    let mut output_line_count = 0usize;
    let mut match_count = 0usize;
    let mut match_limit_reached = false;
    let mut lines_truncated = false;
    let mut limit_hit = false;
    let mut trailing_context_end = 0usize;
    let mut last_file: Option<String> = None;
    let mut done = false;
    let mut killed_due_to_limit = false;

    let push_line = |output_lines: &mut Vec<String>,
                     output_bytes: &mut usize,
                     output_line_count: &mut usize,
                     lines_truncated: &mut bool,
                     last_file: &mut Option<String>,
                     file_path: &str,
                     line_number: usize,
                     line_text: &str,
                     is_match: bool| {
        *last_file = Some(file_path.to_string());
        let relative = format_path(file_path, &search_path, is_directory);
        let sanitized = sanitize_line_text(line_text);
        let (text, was_truncated) = truncate_line(sanitized, GREP_MAX_LINE_LENGTH);
        if was_truncated {
            *lines_truncated = true;
        }
        let line = if is_match {
            format!("{relative}:{line_number}: {text}")
        } else {
            format!("{relative}-{line_number}- {text}")
        };
        if *output_bytes <= DEFAULT_MAX_BYTES + 1 {
            output_lines.push(line.clone());
        }
        *output_bytes += line.len() + 1;
        *output_line_count += 1;
    };

    while let Some(line) = reader
        .next_line()
        .await
        .map_err(|error| format!("Failed to run ripgrep: {error}"))?
    {
        if done || line.trim().is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        let is_match = kind == "match";
        if !is_match && kind != "context" {
            continue;
        }
        let Some(data) = event.get("data") else {
            continue;
        };
        let file_path = decode_rg_text(data.get("path"));
        let Some(line_number) = data.get("line_number").and_then(Value::as_u64) else {
            continue;
        };
        let line_number = line_number as usize;
        if file_path.is_empty() {
            continue;
        }
        let line_text = decode_rg_text(data.get("lines"));

        if !limit_hit {
            push_line(
                &mut output_lines,
                &mut output_bytes,
                &mut output_line_count,
                &mut lines_truncated,
                &mut last_file,
                &file_path,
                line_number,
                &line_text,
                is_match,
            );
            if !is_match {
                continue;
            }
            match_count += 1;
            if match_count >= effective_limit {
                limit_hit = true;
                trailing_context_end = line_number + context_value;
            }
            continue;
        }

        let in_window =
            last_file.as_deref() == Some(file_path.as_str()) && line_number <= trailing_context_end;
        if in_window {
            push_line(
                &mut output_lines,
                &mut output_bytes,
                &mut output_line_count,
                &mut lines_truncated,
                &mut last_file,
                &file_path,
                line_number,
                &line_text,
                false,
            );
        }
        if is_match {
            match_limit_reached = true;
        }
        if match_limit_reached && !(in_window && line_number < trailing_context_end) {
            done = true;
            killed_due_to_limit = true;
            kill_child(&mut child).await;
        }
    }

    let status = child
        .wait()
        .await
        .map_err(|error| format!("Failed to run ripgrep: {error}"))?;
    let stderr = stderr_task.await.unwrap_or_default();

    let code = status.code().unwrap_or(-1);
    if !killed_due_to_limit && code != 0 && code != 1 {
        let message = stderr.trim();
        return Err(if message.is_empty() {
            format!("ripgrep exited with code {code}")
        } else {
            message.to_string()
        });
    }
    if match_count == 0 {
        return Ok(GrepExecution {
            output: "No matches found".to_string(),
            details: None,
        });
    }

    let raw_output = output_lines.join("\n");
    let options = TruncationOptions {
        max_lines: Some(usize::MAX),
        max_bytes: None,
    };
    let mut truncation = truncate_head(&raw_output, &options);
    truncation.total_lines = output_line_count;
    truncation.total_bytes = output_bytes.saturating_sub(1);
    let mut output = truncation.content.clone();
    let mut details = GrepDetails::default();
    let mut notices: Vec<String> = Vec::new();

    if match_limit_reached {
        notices.push(format!(
            "{effective_limit} matches limit reached. Use limit={} for more, or refine pattern",
            effective_limit * 2
        ));
        details.match_limit_reached = Some(effective_limit);
    }
    if truncation.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details.truncation = Some(truncation);
    }
    if lines_truncated {
        notices.push(format!(
            "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
        ));
        details.lines_truncated = true;
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }

    Ok(GrepExecution {
        output,
        details: (details.truncation.is_some()
            || details.match_limit_reached.is_some()
            || details.lines_truncated)
            .then_some(details),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_rg_text_handles_text_and_bytes() {
        let text = serde_json::json!({"text": "hello"});
        assert_eq!(decode_rg_text(Some(&text)), "hello");
        let encoded = base64::engine::general_purpose::STANDARD.encode("héllo");
        let bytes = serde_json::json!({"bytes": encoded});
        assert_eq!(decode_rg_text(Some(&bytes)), "héllo");
        assert_eq!(decode_rg_text(None), "");
    }

    #[test]
    fn format_path_directory_vs_file() {
        let search = Path::new("/work/src");
        assert_eq!(format_path("a.txt", search, true), "a.txt");
        assert_eq!(format_path("/work/src/a.txt", search, true), "a.txt");
        assert_eq!(format_path("/work/src/a.txt", search, false), "a.txt");
    }

    #[test]
    fn sanitize_line_text_keeps_lone_cr() {
        assert_eq!(sanitize_line_text("a\r\n"), "a");
        assert_eq!(sanitize_line_text("a\nb\r"), "a\nb");
    }

    #[tokio::test]
    async fn execute_grep_skips_when_rg_missing() {
        if rg_available() {
            return;
        }
        let input = GrepInput {
            pattern: "x".into(),
            path: None,
            glob: None,
            ignore_case: None,
            literal: None,
            context: None,
            limit: None,
        };
        let error = match execute_grep(&input, Path::new("/"), &LocalGrepOperations).await {
            Ok(_) => return,
            Err(error) => error,
        };
        assert_eq!(
            error,
            "ripgrep (rg) is not available and could not be downloaded"
        );
    }

    #[tokio::test]
    async fn execute_grep_finds_match_when_rg_present() -> std::io::Result<()> {
        if !rg_available() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("f.txt"), "before\nmatch line\nafter\n")?;
        let input = GrepInput {
            pattern: "match".into(),
            path: Some(dir.path().to_string_lossy().into_owned()),
            glob: None,
            ignore_case: None,
            literal: None,
            context: Some(1.0),
            limit: None,
        };
        let result = execute_grep(&input, Path::new("/"), &LocalGrepOperations)
            .await
            .map_err(std::io::Error::other)?;
        assert!(result.output.contains("f.txt:2: match line"));
        assert!(result.output.contains("f.txt-1- before"));
        assert!(result.output.contains("f.txt-3- after"));
        Ok(())
    }
}
