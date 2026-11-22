//! File search by glob pattern, ported from `core/tools/find.ts`.
//!
//! Upstream shells out to `fd`; this port uses the `ignore` walker plus `globset`
//! so the tool works without an external binary. It skips `node_modules` and
//! `.git`, honors `.gitignore`, and includes hidden files like `fd --hidden`.

use std::path::{Component, Path, PathBuf};
use std::process::Command;

use globset::{Glob, GlobBuilder};
use ignore::WalkBuilder;
use serde::Deserialize;

use super::path_utils::resolve_to_cwd;
use super::truncate::{
    DEFAULT_MAX_BYTES, TruncationOptions, TruncationResult, format_size, truncate_head,
};

pub const DEFAULT_LIMIT: usize = 1000;

#[derive(Clone, Debug, Deserialize)]
pub struct FindInput {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub limit: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FindDetails {
    pub truncation: Option<TruncationResult>,
    pub result_limit_reached: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct FindExecution {
    pub output: String,
    pub details: Option<FindDetails>,
}

pub trait FindOperations {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    /// Return matching paths (absolute or relative); at most `limit` results.
    fn glob(&self, pattern: &str, cwd: &Path, ignore: &[String], limit: usize) -> Vec<PathBuf>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LocalFindOperations;

impl FindOperations for LocalFindOperations {
    fn glob(&self, pattern: &str, cwd: &Path, ignore: &[String], limit: usize) -> Vec<PathBuf> {
        glob_find(pattern, cwd, ignore, limit)
    }
}

/// Lexical `path.relative(from, to)`, shared by the find and grep tools.
pub fn path_relative(from: &Path, to: &Path) -> PathBuf {
    let from_parts: Vec<Component<'_>> = from.components().collect();
    let to_parts: Vec<Component<'_>> = to.components().collect();
    let mut common = 0;
    while common < from_parts.len()
        && common < to_parts.len()
        && from_parts[common] == to_parts[common]
    {
        common += 1;
    }

    let mut out = PathBuf::new();
    for _ in common..from_parts.len() {
        out.push("..");
    }
    for component in &to_parts[common..] {
        out.push(component.as_os_str());
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// `relativizeFindResultPath`: normalize separators to `/`, keep a trailing `/`.
pub fn relativize_find_result_path(result_path: &str, search_path: &Path) -> String {
    let had_trailing_separator = result_path.ends_with('/') || result_path.ends_with('\\');
    let result = Path::new(result_path);
    let relative = if result.is_absolute() {
        path_relative(search_path, result)
    } else {
        result.to_path_buf()
    };
    let posix = relative.to_string_lossy().replace('\\', "/");
    if had_trailing_separator && !posix.ends_with('/') {
        format!("{posix}/")
    } else {
        posix
    }
}

fn matcher_for(pattern: &str) -> Option<globset::GlobMatcher> {
    let ignore_case = pattern == pattern.to_lowercase();
    GlobBuilder::new(pattern)
        .case_insensitive(ignore_case)
        .literal_separator(true)
        .build()
        .ok()
        .map(|glob: Glob| glob.compile_matcher())
}

fn is_ignored_dir(name: &str) -> bool {
    name == ".git" || name == "node_modules"
}

/// Glob-based search backing `LocalFindOperations::glob`.
pub fn glob_find(pattern: &str, cwd: &Path, ignore: &[String], limit: usize) -> Vec<PathBuf> {
    let _ = ignore; // The walker prunes the fixed node_modules/.git set directly.
    if limit == 0 {
        return Vec::new();
    }
    let Some(matcher) = matcher_for(pattern) else {
        return Vec::new();
    };
    let has_separator = pattern.contains('/');

    let mut results = Vec::new();
    let walker = WalkBuilder::new(cwd)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .filter_entry(|entry| {
            if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                let name = entry.file_name().to_string_lossy();
                !is_ignored_dir(&name)
            } else {
                true
            }
        })
        .build();

    for entry in walker.flatten() {
        let path = entry.path();
        if path == cwd {
            continue;
        }

        let relative = path_relative(cwd, path);
        let relative_string = relative.to_string_lossy().replace('\\', "/");
        let matched = if has_separator {
            matcher.is_match(&relative_string)
        } else {
            path.file_name()
                .is_some_and(|name| matcher.is_match(name.to_string_lossy().as_ref()))
        };
        if matched {
            results.push(path.to_path_buf());
            if results.len() >= limit {
                break;
            }
        }
    }
    results
}

pub fn fd_available() -> bool {
    Command::new("fd")
        .arg("--version")
        .status()
        .is_ok_and(|status| status.success())
}

/// Execute the find tool and return its model-facing text plus optional details.
pub fn execute_find(
    input: &FindInput,
    cwd: &Path,
    ops: &dyn FindOperations,
) -> Result<FindExecution, String> {
    let search_path = resolve_to_cwd(input.path.as_deref().unwrap_or("."), cwd);
    let effective_limit = input.limit.unwrap_or(DEFAULT_LIMIT as f64).floor().max(1.0) as usize;

    if !ops.exists(&search_path) {
        return Err(format!("Path not found: {}", search_path.display()));
    }

    let results = ops.glob(
        &input.pattern,
        &search_path,
        &["**/node_modules/**".to_string(), "**/.git/**".to_string()],
        effective_limit + 1,
    );
    if results.is_empty() {
        return Ok(FindExecution {
            output: "No files found matching pattern".to_string(),
            details: None,
        });
    }

    let result_limit_reached = results.len() > effective_limit;
    let relativized: Vec<String> = results
        .iter()
        .take(effective_limit)
        .map(|path| relativize_find_result_path(&path.to_string_lossy(), &search_path))
        .collect();
    let raw_output = relativized.join("\n");
    let options = TruncationOptions {
        max_lines: Some(usize::MAX),
        max_bytes: None,
    };
    let truncation = truncate_head(&raw_output, &options);
    let mut output = truncation.content.clone();
    let mut details = FindDetails::default();
    let mut notices: Vec<String> = Vec::new();

    if result_limit_reached {
        notices.push(format!("{effective_limit} results limit reached"));
        details.result_limit_reached = Some(effective_limit);
    }
    if truncation.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details.truncation = Some(truncation);
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }

    Ok(FindExecution {
        output,
        details: (details.truncation.is_some() || details.result_limit_reached.is_some())
            .then_some(details),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relativize_absolute_and_trailing_separator() {
        assert_eq!(
            relativize_find_result_path("/work/project/src/a.ts", Path::new("/work/project")),
            "src/a.ts"
        );
        assert_eq!(
            relativize_find_result_path("/work/project/src/", Path::new("/work/project")),
            "src/"
        );
        assert_eq!(
            relativize_find_result_path("a/b.txt", Path::new("/work")),
            "a/b.txt"
        );
    }

    #[test]
    fn path_relative_uses_dotdot_for_siblings() -> std::io::Result<()> {
        let _ = std::fs::metadata(".")?;
        assert_eq!(
            path_relative(Path::new("/a/b"), Path::new("/a/c")),
            PathBuf::from("../c")
        );
        assert_eq!(
            path_relative(Path::new("/a/b"), Path::new("/a/b")),
            PathBuf::from(".")
        );
        Ok(())
    }

    #[test]
    fn glob_find_matches_nested_and_skips_ignored_dirs() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(dir.path().join("src/nested"))?;
        std::fs::create_dir_all(dir.path().join("node_modules/pkg"))?;
        std::fs::create_dir_all(dir.path().join(".git"))?;
        std::fs::write(dir.path().join("src/a.ts"), "")?;
        std::fs::write(dir.path().join("src/nested/b.ts"), "")?;
        std::fs::write(dir.path().join("node_modules/pkg/c.ts"), "")?;
        std::fs::write(dir.path().join(".git/d.ts"), "")?;

        let results = glob_find("src/**/*.ts", dir.path(), &[], 100);
        let display: Vec<String> = results
            .iter()
            .map(|p| {
                path_relative(dir.path(), p)
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert!(display.contains(&"src/a.ts".to_string()));
        assert!(display.contains(&"src/nested/b.ts".to_string()));
        assert!(
            !display
                .iter()
                .any(|p| p.contains("node_modules") || p.contains(".git"))
        );

        let basename_results = glob_find("*.ts", dir.path(), &[], 100);
        assert_eq!(basename_results.len(), 2);
        Ok(())
    }

    #[test]
    fn execute_find_reports_limit() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(dir.path().join(name), "")?;
        }
        let input = FindInput {
            pattern: "*.txt".into(),
            path: Some(dir.path().to_string_lossy().into_owned()),
            limit: Some(2.0),
        };
        let result = execute_find(&input, Path::new("/"), &LocalFindOperations)
            .map_err(std::io::Error::other)?;
        assert!(result.output.contains("2 results limit reached"));
        assert_eq!(result.details.and_then(|d| d.result_limit_reached), Some(2));
        Ok(())
    }
}
