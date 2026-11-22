//! Directory listing tool ported from `core/tools/ls.ts`.

use std::path::Path;

use serde::Deserialize;

use super::path_utils::resolve_to_cwd;
use super::truncate::{
    DEFAULT_MAX_BYTES, TruncationOptions, TruncationResult, format_size, truncate_head,
};

pub const DEFAULT_LIMIT: usize = 500;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct LsInput {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub limit: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LsDetails {
    pub truncation: Option<TruncationResult>,
    pub entry_limit_reached: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct LsExecution {
    pub output: String,
    pub details: Option<LsDetails>,
}

/// Execute the ls tool and return its model-facing text plus optional details.
pub fn execute_ls(input: &LsInput, cwd: &Path) -> Result<LsExecution, String> {
    if !path_exists_sync(cwd) {
        return Err(format!(
            "Working directory does not exist: {}",
            cwd.display()
        ));
    }

    let dir_path = resolve_to_cwd(input.path.as_deref().unwrap_or("."), cwd);
    let effective_limit = input.limit.unwrap_or(DEFAULT_LIMIT as f64).floor().max(1.0) as usize;

    if !path_exists_sync(&dir_path) {
        return Err(format!("Path not found: {}", dir_path.display()));
    }
    let metadata = std::fs::metadata(&dir_path)
        .map_err(|_| format!("Path not found: {}", dir_path.display()))?;
    if !metadata.is_dir() {
        return Err(format!("Not a directory: {}", dir_path.display()));
    }

    let read_dir =
        std::fs::read_dir(&dir_path).map_err(|error| format!("Cannot read directory: {error}"))?;
    let mut entries: Vec<String> = Vec::new();
    for entry in read_dir.flatten() {
        entries.push(entry.file_name().to_string_lossy().into_owned());
    }
    entries.sort_by_key(|entry| entry.to_lowercase());

    let mut results: Vec<String> = Vec::new();
    let mut entry_limit_reached = false;
    for entry in entries {
        if results.len() >= effective_limit {
            entry_limit_reached = true;
            break;
        }
        let full_path = dir_path.join(&entry);
        let Ok(entry_metadata) = std::fs::metadata(&full_path) else {
            continue;
        };
        let suffix = if entry_metadata.is_dir() { "/" } else { "" };
        results.push(format!("{entry}{suffix}"));
    }

    if results.is_empty() {
        return Ok(LsExecution {
            output: "(empty directory)".to_string(),
            details: None,
        });
    }

    let raw_output = results.join("\n");
    let options = TruncationOptions {
        max_lines: Some(usize::MAX),
        max_bytes: None,
    };
    let truncation = truncate_head(&raw_output, &options);
    let mut output = truncation.content.clone();
    let mut details = LsDetails::default();
    let mut notices: Vec<String> = Vec::new();

    if entry_limit_reached {
        notices.push(format!(
            "{effective_limit} entries limit reached. Use limit={} for more",
            effective_limit * 2
        ));
        details.entry_limit_reached = Some(effective_limit);
    }
    if truncation.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details.truncation = Some(truncation);
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }

    Ok(LsExecution {
        output,
        details: (details.truncation.is_some() || details.entry_limit_reached.is_some())
            .then_some(details),
    })
}

fn path_exists_sync(path: &Path) -> bool {
    std::fs::metadata(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_entries_sorted_with_directory_suffix() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("b.txt"), "")?;
        std::fs::write(dir.path().join("A.txt"), "")?;
        std::fs::create_dir(dir.path().join(".hidden-dir"))?;
        let input = LsInput {
            path: None,
            limit: None,
        };
        let result = execute_ls(&input, dir.path()).map_err(std::io::Error::other)?;
        assert_eq!(result.output, ".hidden-dir/\nA.txt\nb.txt");
        Ok(())
    }

    #[test]
    fn empty_directory_message() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = LsInput {
            path: None,
            limit: None,
        };
        assert_eq!(
            execute_ls(&input, dir.path())
                .map_err(std::io::Error::other)?
                .output,
            "(empty directory)"
        );
        Ok(())
    }

    #[test]
    fn entry_limit_notice() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(dir.path().join(name), "")?;
        }
        let input = LsInput {
            path: None,
            limit: Some(1.9),
        };
        let result = execute_ls(&input, dir.path()).map_err(std::io::Error::other)?;
        assert_eq!(
            result.output,
            "a.txt\n\n[1 entries limit reached. Use limit=2 for more]"
        );
        assert_eq!(result.details.and_then(|d| d.entry_limit_reached), Some(1));
        Ok(())
    }

    #[test]
    fn missing_and_non_directory_errors() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let missing = LsInput {
            path: Some("nope".into()),
            limit: None,
        };
        let error = match execute_ls(&missing, dir.path()) {
            Ok(_) => return Err(std::io::Error::other("expected missing-path error")),
            Err(error) => error,
        };
        assert!(error.starts_with("Path not found:"));
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "x")?;
        let not_dir = LsInput {
            path: Some("f.txt".into()),
            limit: None,
        };
        let error = match execute_ls(&not_dir, dir.path()) {
            Ok(_) => return Err(std::io::Error::other("expected not-a-directory error")),
            Err(error) => error,
        };
        assert!(error.starts_with("Not a directory:"));
        Ok(())
    }
}
