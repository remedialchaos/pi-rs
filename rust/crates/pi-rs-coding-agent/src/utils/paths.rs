//! Path normalization helpers, ported from `packages/coding-agent/src/utils/paths.ts`.
//!
//! Only the lexical parts are ported: tilde expansion, Unicode-space normalization,
//! stripping a single leading `@`, `file://` URL conversion, and resolving against a
//! base directory. Filesystem-touching helpers (`canonicalizePath`, `getFileRevision`,
//! etc.) are intentionally out of scope.

use std::env;
use std::path::{Component, Path, PathBuf};
use url::Url;

/// Unicode space variants normalized to the regular ASCII space by upstream.
const UNICODE_SPACES: &[char] = &[
    '\u{00a0}', '\u{2000}', '\u{2001}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}', '\u{2006}',
    '\u{2007}', '\u{2008}', '\u{2009}', '\u{200a}', '\u{202f}', '\u{205f}', '\u{3000}',
];

/// Normalize a path string: replace Unicode space variants, strip one leading `@`,
/// expand a leading `~`, and convert `file://` URLs to filesystem paths.
///
/// Upstream makes Unicode-space and `@` handling opt-in; this port applies them
/// unconditionally to match the calling convention used by the CLI file processor.
pub fn normalize_path(input: &str) -> String {
    let mut normalized: String = input
        .chars()
        .map(|c| if UNICODE_SPACES.contains(&c) { ' ' } else { c })
        .collect();

    if let Some(stripped) = normalized.strip_prefix('@') {
        normalized = stripped.to_string();
    }

    if let Some(home) = home_dir() {
        if normalized == "~" {
            return home.to_string_lossy().into_owned();
        }
        if let Some(rest) = tilde_rest(&normalized) {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }

    if normalized.starts_with("file://")
        && let Ok(url) = Url::parse(&normalized)
        && let Ok(path) = url.to_file_path()
    {
        return path.to_string_lossy().into_owned();
    }

    normalized
}

/// Resolve `input` against `base`. When `trim` is true the input is trimmed before
/// normalization (used by settings-source paths in the TUI). A relative path is
/// joined to `base`; an absolute path replaces it. `.` and `..` components are
/// resolved lexically, without touching the filesystem.
pub fn resolve_path(input: &str, base: &Path, trim: bool) -> PathBuf {
    let input = if trim { input.trim() } else { input };
    let normalized_input = normalize_path(input);
    let normalized_base = normalize_path(&base.to_string_lossy());

    let path = PathBuf::from(normalized_input);
    if path.is_absolute() {
        lexical_normalize(&path)
    } else {
        lexical_normalize(&PathBuf::from(normalized_base).join(path))
    }
}

/// Lexically resolve `.` and `..` without accessing the filesystem.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => result.push(prefix.as_os_str()),
            Component::RootDir => result.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                let can_pop = matches!(result.components().next_back(), Some(Component::Normal(_)));
                if can_pop {
                    result.pop();
                } else if !result.has_root() {
                    result.push("..");
                }
            }
            Component::Normal(part) => result.push(part),
        }
    }
    if result.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        result
    }
}

/// Return the portion of `value` following a recognized `~/` prefix.
fn tilde_rest(value: &str) -> Option<&str> {
    let posix = value.strip_prefix("~/");
    #[cfg(windows)]
    {
        posix.or_else(|| value.strip_prefix("~\\"))
    }
    #[cfg(not(windows))]
    {
        posix
    }
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let value = env::var_os("USERPROFILE").or_else(|| env::var_os("HOME"));
    #[cfg(not(windows))]
    let value = env::var_os("HOME");
    value
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_at_prefix_and_normalizes_unicode_spaces() {
        assert_eq!(normalize_path("@file\u{00a0}name"), "file name");
        assert_eq!(normalize_path("a\u{2003}b\u{3000}c"), "a b c");
        assert_eq!(normalize_path("@@name"), "@name");
    }

    #[test]
    fn converts_file_urls() {
        assert_eq!(
            normalize_path("file:///tmp/example.txt"),
            "/tmp/example.txt"
        );
        assert_eq!(normalize_path("file:///tmp/a%20b.txt"), "/tmp/a b.txt");
    }

    #[test]
    fn resolves_relative_paths_against_base() {
        let base = Path::new("/work/project");
        assert_eq!(
            resolve_path("src/./main.rs", base, false),
            PathBuf::from("/work/project/src/main.rs")
        );
        assert_eq!(
            resolve_path("../other", base, false),
            PathBuf::from("/work/other")
        );
        assert_eq!(
            resolve_path("  ./x  ", base, true),
            PathBuf::from("/work/project/x")
        );
        assert_eq!(
            resolve_path("/abs/path", base, false),
            PathBuf::from("/abs/path")
        );
    }

    #[test]
    fn expands_tilde() {
        if let Some(home) = home_dir() {
            assert_eq!(
                normalize_path("~/notes.txt"),
                home.join("notes.txt").to_string_lossy()
            );
        }
    }
}
