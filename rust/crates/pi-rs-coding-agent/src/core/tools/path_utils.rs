//! Path helpers mirroring `core/tools/path-utils.ts` and `utils/paths.ts`.
//!
//! There is intentionally no cwd/sandbox confinement: upstream uses `path.resolve`,
//! so absolute paths and `..` segments are accepted verbatim.

use std::path::{Component, Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

/// Unicode space variants normalized to an ASCII space by `normalizePath`.
fn is_unicode_space(c: char) -> bool {
    matches!(
        c,
        '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
    )
}

fn home_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home);
    }
    #[cfg(windows)]
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(profile);
    }
    PathBuf::from("/")
}

/// `normalizePath` with the options used by the tools: normalize unicode spaces,
/// strip one leading `@`, expand a leading `~`, and decode `file://` URLs.
fn normalize_path_string(input: &str) -> String {
    let mut normalized: String = input
        .chars()
        .map(|c| if is_unicode_space(c) { ' ' } else { c })
        .collect();

    if let Some(rest) = normalized.strip_prefix('@') {
        normalized = rest.to_string();
    }

    if normalized == "~" {
        return home_dir().to_string_lossy().into_owned();
    }
    if let Some(rest) = normalized.strip_prefix("~/") {
        return home_dir().join(rest).to_string_lossy().into_owned();
    }
    #[cfg(windows)]
    if let Some(rest) = normalized.strip_prefix("~\\") {
        return home_dir().join(rest).to_string_lossy().into_owned();
    }

    if normalized.starts_with("file://")
        && let Some(path) = file_url_to_path(&normalized)
    {
        return path.to_string_lossy().into_owned();
    }

    normalized
}

fn file_url_to_path(input: &str) -> Option<PathBuf> {
    let url = url::Url::parse(input).ok()?;
    if url.scheme() != "file" {
        return None;
    }
    url.to_file_path().ok()
}

/// Lexical equivalent of node's `path.resolve` component collapse (no filesystem access).
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(std::path::MAIN_SEPARATOR.to_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// `pathExists` from path-utils.ts (async).
pub async fn path_exists(path: &Path) -> bool {
    tokio::fs::metadata(path).await.is_ok()
}

/// `expandPath`: normalize a user-supplied path without resolving it against a cwd.
pub fn expand_path(input: &str) -> String {
    normalize_path_string(input)
}

/// `resolveToCwd`: normalize then resolve relative to `cwd` (absolute paths win).
pub fn resolve_to_cwd(input: &str, cwd: &Path) -> PathBuf {
    let normalized = normalize_path_string(input);
    let candidate = Path::new(&normalized);
    if candidate.is_absolute() {
        lexical_normalize(candidate)
    } else {
        lexical_normalize(&cwd.join(candidate))
    }
}

fn macos_screenshot_variant(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len() + 4);
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ' '
            && i + 3 < chars.len()
            && matches!(chars[i + 1].to_ascii_lowercase(), 'a' | 'p')
            && chars[i + 2].eq_ignore_ascii_case(&'m')
            && chars[i + 3] == '.'
        {
            out.push('\u{202F}');
            for ch in &chars[i + 1..=i + 3] {
                out.push(*ch);
            }
            i += 4;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

fn nfd_variant(input: &str) -> String {
    input.nfd().collect()
}

fn curly_quote_variant(input: &str) -> String {
    input.replace('\'', "\u{2019}")
}

/// `resolveReadPath`: first-existing fallback chain (screenshot NBSP, NFD,
/// curly quotes, NFD + curly quotes), otherwise the plain resolved path.
pub fn resolve_read_path(input: &str, cwd: &Path) -> PathBuf {
    let resolved = resolve_to_cwd(input, cwd);
    let resolved_string = resolved.to_string_lossy().into_owned();

    if std::fs::metadata(&resolved).is_ok() {
        return resolved;
    }

    let am_pm = macos_screenshot_variant(&resolved_string);
    if am_pm != resolved_string {
        let candidate = PathBuf::from(&am_pm);
        if std::fs::metadata(&candidate).is_ok() {
            return candidate;
        }
    }

    let nfd = nfd_variant(&resolved_string);
    if nfd != resolved_string {
        let candidate = PathBuf::from(&nfd);
        if std::fs::metadata(&candidate).is_ok() {
            return candidate;
        }
    }

    let curly = curly_quote_variant(&resolved_string);
    if curly != resolved_string {
        let candidate = PathBuf::from(&curly);
        if std::fs::metadata(&candidate).is_ok() {
            return candidate;
        }
    }

    let nfd_curly = curly_quote_variant(&nfd);
    if nfd_curly != resolved_string {
        let candidate = PathBuf::from(&nfd_curly);
        if std::fs::metadata(&candidate).is_ok() {
            return candidate;
        }
    }

    resolved
}

/// Async counterpart of `resolve_read_path`.
pub async fn resolve_read_path_async(input: &str, cwd: &Path) -> PathBuf {
    let resolved = resolve_to_cwd(input, cwd);
    let resolved_string = resolved.to_string_lossy().into_owned();

    if path_exists(&resolved).await {
        return resolved;
    }

    let am_pm = macos_screenshot_variant(&resolved_string);
    if am_pm != resolved_string {
        let candidate = PathBuf::from(&am_pm);
        if path_exists(&candidate).await {
            return candidate;
        }
    }

    let nfd = nfd_variant(&resolved_string);
    if nfd != resolved_string {
        let candidate = PathBuf::from(&nfd);
        if path_exists(&candidate).await {
            return candidate;
        }
    }

    let curly = curly_quote_variant(&resolved_string);
    if curly != resolved_string {
        let candidate = PathBuf::from(&curly);
        if path_exists(&candidate).await {
            return candidate;
        }
    }

    let nfd_curly = curly_quote_variant(&nfd);
    if nfd_curly != resolved_string {
        let candidate = PathBuf::from(&nfd_curly);
        if path_exists(&candidate).await {
            return candidate;
        }
    }

    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_tilde_only_for_exact_tilde_or_tilde_slash() {
        assert!(!expand_path("~").contains('~'));
        assert!(!expand_path("~/Documents/file.txt").contains("~/"));
        assert_eq!(expand_path("@~draft.md"), "~draft.md");
        assert_eq!(expand_path("file\u{a0}name.txt"), "file name.txt");
    }

    #[test]
    fn resolve_to_cwd_absolute_and_relative() {
        let cwd = Path::new("/work/project");
        assert_eq!(
            resolve_to_cwd("/etc/hosts", cwd),
            PathBuf::from("/etc/hosts")
        );
        assert_eq!(
            resolve_to_cwd("src/main.rs", cwd),
            PathBuf::from("/work/project/src/main.rs")
        );
        assert_eq!(
            resolve_to_cwd("@~draft.md", cwd),
            PathBuf::from("/work/project/~draft.md")
        );
        assert_eq!(
            resolve_to_cwd("../other", cwd),
            PathBuf::from("/work/other")
        );
    }

    #[test]
    fn resolve_read_path_prefers_existing_file() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("plain.txt");
        std::fs::write(&file, "x")?;
        assert_eq!(resolve_read_path("plain.txt", dir.path()), file);
        Ok(())
    }

    #[test]
    fn resolve_read_path_nfd_variant() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let stored = "café.txt";
        let nfd_name: String = stored.nfd().collect();
        let file = dir.path().join(&nfd_name);
        std::fs::write(&file, "x")?;
        assert_eq!(resolve_read_path(stored, dir.path()), file);
        Ok(())
    }

    #[test]
    fn resolve_read_path_curly_quote_variant() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let stored = "d’écran.png";
        let file = dir.path().join(stored);
        std::fs::write(&file, "x")?;
        assert_eq!(resolve_read_path("d'écran.png", dir.path()), file);
        Ok(())
    }

    #[test]
    fn resolve_read_path_macos_screenshot_variant() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let stored = "Screenshot 2024-01-01 at 10.00.00 AM.png";
        let file = dir.path().join(stored);
        std::fs::write(&file, "x")?;
        assert_eq!(
            resolve_read_path("Screenshot 2024-01-01 at 10.00.00 AM.png", dir.path()),
            file
        );
        let lower = "Screenshot 2024-01-01 at 10.00.00 pm.png";
        let lower_file = dir.path().join(lower);
        std::fs::write(&lower_file, "x")?;
        assert_eq!(
            resolve_read_path("Screenshot 2024-01-01 at 10.00.00 pm.png", dir.path()),
            lower_file
        );
        Ok(())
    }

    #[test]
    fn macos_variant_replaces_only_space_before_am_pm_dot() {
        assert_eq!(macos_screenshot_variant("at 3 PM."), "at 3 PM.");
        assert_eq!(macos_screenshot_variant("at 3 pm!"), "at 3 pm!");
    }
}
