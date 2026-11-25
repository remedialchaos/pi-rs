//! Changelog parsing and link normalization: the Rust port of `packages/coding-agent/src/utils/changelog.ts`.

use std::path::Path;

const GITHUB_REPO: &str = "earendil-works/pi";
const BASE_PATH: &str = "packages/coding-agent";
const LEGACY_PREFIXES: [&str; 2] = [
    "https://github.com/badlogic/pi-mono",
    "https://github.com/earendil-works/pi-mono",
];

/// One version section of `CHANGELOG.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangelogEntry {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub content: String,
}

impl ChangelogEntry {
    fn version(&self) -> String {
        format!("{}.{}.{}", self.major, self.minor, self.patch)
    }
}

fn normalize_tag(version: &str) -> String {
    if version.starts_with('v') {
        version.to_string()
    } else {
        format!("v{version}")
    }
}

fn posix_normalize(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let absolute = path.starts_with('/');
    let trailing = path.ends_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let mut result = parts.join("/");
    if absolute {
        result = format!("/{result}");
    }
    if result.is_empty() {
        return if absolute { "/" } else { "." }.to_string();
    }
    if trailing && !result.ends_with('/') {
        result.push('/');
    }
    result
}

fn encode_uri(text: &str) -> String {
    let mut out = String::new();
    for byte in text.as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'();/?:@&=+$,#".contains(byte) {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn split_target(target: &str) -> (&str, &str, &str) {
    let (before, fragment) = match target.find('#') {
        Some(index) => (&target[..index], &target[index..]),
        None => (target, ""),
    };
    match before.find('?') {
        Some(index) => (&before[..index], &before[index..], fragment),
        None => (before, "", fragment),
    }
}

fn has_url_scheme(target: &str) -> bool {
    let mut bytes = target.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    let mut valid = true;
    for byte in bytes {
        if byte == b':' {
            return valid;
        }
        valid &= byte.is_ascii_alphanumeric() || b"+.-".contains(&byte);
    }
    false
}

/// Rewrites one Markdown link target to a tag-pinned GitHub URL where appropriate.
pub fn normalize_link_target(target: &str, tag: &str) -> String {
    let repository = format!("https://github.com/{GITHUB_REPO}");
    let mut canonical = target.to_string();
    for prefix in LEGACY_PREFIXES {
        if canonical == prefix || canonical.starts_with(&format!("{prefix}/")) {
            canonical = format!("{repository}{}", &canonical[prefix.len()..]);
            break;
        }
    }
    for route in ["blob", "tree"] {
        for branch in ["main", "master"] {
            let prefix = format!("{repository}/{route}/{branch}/");
            if let Some(rest) = canonical.strip_prefix(&prefix) {
                canonical = format!("{repository}/{route}/{tag}/{rest}");
            }
        }
    }
    if canonical.starts_with('#') || canonical.starts_with("//") || has_url_scheme(&canonical) {
        return canonical;
    }
    let (path_part, query, fragment) = split_target(&canonical);
    if path_part.is_empty() {
        return canonical;
    }
    let normalized = path_part.replace('\\', "/");
    let joined = match normalized.strip_prefix('/') {
        Some(stripped) => posix_normalize(stripped),
        None => posix_normalize(&format!("{BASE_PATH}/{normalized}")),
    };
    if joined == "." || joined == ".." || joined.starts_with("../") {
        return canonical;
    }
    let directory =
        path_part.ends_with('/') || !joined.rsplit('/').next().unwrap_or(&joined).contains('.');
    let route = if directory { "tree" } else { "blob" };
    format!(
        "{repository}/{route}/{tag}/{}{query}{fragment}",
        encode_uri(&joined)
    )
}

/// Rewrites all inline Markdown links in `markdown` for the given version (string or entry).
pub fn normalize_changelog_links(markdown: &str, version: &str) -> String {
    normalize_links(markdown, &normalize_tag(version))
}

/// Same as [`normalize_changelog_links`] with a structured version.
pub fn normalize_changelog_links_for_entry(markdown: &str, entry: &ChangelogEntry) -> String {
    normalize_links(markdown, &normalize_tag(&entry.version()))
}

fn normalize_links(markdown: &str, tag: &str) -> String {
    let chars: Vec<(usize, char)> = markdown.char_indices().collect();
    let count = chars.len();
    let mut out = String::with_capacity(markdown.len());
    let mut copied = 0;
    let mut at = 0;
    while at < count {
        let mut cursor = at;
        if chars[cursor].1 == '!' {
            cursor += 1;
        }
        if cursor >= count || chars[cursor].1 != '[' {
            at += 1;
            continue;
        }
        cursor += 1;
        let label_start = cursor;
        while cursor < count && chars[cursor].1 != ']' && chars[cursor].1 != '\n' {
            cursor += 1;
        }
        if label_start == cursor || cursor >= count || chars[cursor].1 != ']' {
            at += 1;
            continue;
        }
        cursor += 1;
        if cursor >= count || chars[cursor].1 != '(' {
            at += 1;
            continue;
        }
        cursor += 1;
        let prefix_end = cursor;
        let target_start = cursor;
        while cursor < count && !chars[cursor].1.is_whitespace() && chars[cursor].1 != ')' {
            cursor += 1;
        }
        if target_start == cursor {
            at += 1;
            continue;
        }
        let target_end = cursor;
        let suffix_start = cursor;
        if cursor < count && chars[cursor].1.is_whitespace() {
            while cursor < count && chars[cursor].1.is_whitespace() {
                cursor += 1;
            }
            while cursor < count && chars[cursor].1 != ')' {
                cursor += 1;
            }
        }
        if cursor >= count || chars[cursor].1 != ')' {
            at += 1;
            continue;
        }
        cursor += 1;
        out.push_str(&markdown[copied..chars[at].0]);
        out.push_str(&markdown[chars[at].0..chars[prefix_end].0]);
        let target = &markdown[chars[target_start].0..chars[target_end].0];
        out.push_str(&normalize_link_target(target, tag));
        let end = chars[cursor - 1].0 + chars[cursor - 1].1.len_utf8();
        out.push_str(&markdown[chars[suffix_start].0..end]);
        copied = end;
        at = cursor;
    }
    out.push_str(&markdown[copied..]);
    out
}

/// Parses version sections from a changelog file. Unreadable files yield no entries.
pub fn parse_changelog(path: &Path) -> Vec<ChangelogEntry> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let content = String::from_utf8_lossy(&bytes);
    let mut entries = Vec::new();
    let mut current: Option<(u64, u64, u64)> = None;
    let mut lines: Vec<String> = Vec::new();
    for line in content.split('\n') {
        if line.starts_with("## ") {
            if let Some((major, minor, patch)) = current.take()
                && !lines.is_empty()
            {
                entries.push(ChangelogEntry {
                    major,
                    minor,
                    patch,
                    content: lines.join("\n").trim().to_string(),
                });
            }
            lines.clear();
            current = parse_header(line);
            if current.is_some() {
                lines.push(line.to_string());
            }
        } else if current.is_some() {
            lines.push(line.to_string());
        }
    }
    if let Some((major, minor, patch)) = current
        && !lines.is_empty()
    {
        entries.push(ChangelogEntry {
            major,
            minor,
            patch,
            content: lines.join("\n").trim().to_string(),
        });
    }
    entries
}

fn parse_header(line: &str) -> Option<(u64, u64, u64)> {
    let bytes = line.as_bytes();
    let mut at = 2;
    while at < bytes.len() && (bytes[at] as char).is_whitespace() {
        at += 1;
    }
    if at == 2 {
        return None;
    }
    if bytes.get(at) == Some(&b'[') {
        at += 1;
    }
    let mut numbers = [0_u64; 3];
    for (index, number) in numbers.iter_mut().enumerate() {
        let start = at;
        while at < bytes.len() && bytes[at].is_ascii_digit() {
            at += 1;
        }
        if start == at {
            return None;
        }
        *number = line[start..at].parse().unwrap_or(0);
        if index < 2 {
            if bytes.get(at) != Some(&b'.') {
                return None;
            }
            at += 1;
        }
    }
    Some((numbers[0], numbers[1], numbers[2]))
}

/// Signed numeric comparison: -1, 0, or 1 (the upstream returns the raw difference).
pub fn compare_versions(left: &ChangelogEntry, right: &ChangelogEntry) -> i64 {
    for (a, b) in [
        (left.major, right.major),
        (left.minor, right.minor),
        (left.patch, right.patch),
    ] {
        if a != b {
            return a as i64 - b as i64;
        }
    }
    0
}

/// Entries newer than `last_version`; `None` when the version has no major.minor.patch.
pub fn get_new_entries(
    entries: &[ChangelogEntry],
    last_version: &str,
) -> Option<Vec<ChangelogEntry>> {
    let text = last_version
        .trim()
        .strip_prefix('v')
        .unwrap_or(last_version.trim());
    let mut numbers = [0_u64; 3];
    let mut at = 0;
    let bytes = text.as_bytes();
    for (index, number) in numbers.iter_mut().enumerate() {
        let start = at;
        while at < bytes.len() && bytes[at].is_ascii_digit() {
            at += 1;
        }
        if start == at {
            return None;
        }
        *number = text[start..at].parse().unwrap_or(0);
        if index < 2 {
            if bytes.get(at) != Some(&b'.') {
                return None;
            }
            at += 1;
        }
    }
    let last = ChangelogEntry {
        major: numbers[0],
        minor: numbers[1],
        patch: numbers[2],
        content: String::new(),
    };
    Some(
        entries
            .iter()
            .filter(|entry| compare_versions(entry, &last) > 0)
            .cloned()
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_package_relative_links() {
        let markdown = "[Root](../../README.md#supply-chain-hardening)";
        assert_eq!(
            normalize_changelog_links(markdown, "0.79.0"),
            "[Root](https://github.com/earendil-works/pi/blob/v0.79.0/README.md#supply-chain-hardening)"
        );
    }

    #[test]
    fn ignores_prerelease_suffixes() {
        let entries = vec![ChangelogEntry {
            major: 0,
            minor: 1,
            patch: 5,
            content: "0.1.5".into(),
        }];
        assert!(
            get_new_entries(&entries, "0.1.5-20261020.7")
                .unwrap()
                .is_empty()
        );
        assert_eq!(get_new_entries(&entries, "0.1.4-beta.1").unwrap().len(), 1);
        assert!(get_new_entries(&entries, "latest").is_none());
    }
}
