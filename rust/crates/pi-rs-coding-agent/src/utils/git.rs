//! Git source parsing, port of `packages/coding-agent/src/utils/git.ts`.
//!
//! Upstream delegates host recognition to `hosted-git-info`. This port implements
//! the equivalent behavior for its four default known hosts (github.com,
//! gitlab.com, bitbucket.org, gist.github.com) using a small URL parser. Unknown
//! hosts fall through to the generic parser, where the same safety checks reject
//! traversal, absolute paths, backslashes, NUL bytes and malformed escapes.

use serde::Serialize;
use url::Url;

/// Parsed git source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitSource {
    /// Always `"git"` for git sources (upstream field name is `type`).
    #[serde(rename = "type")]
    pub source_type: String,
    /// Clone URL, without a ref suffix.
    pub repo: String,
    /// Git host domain, e.g. `github.com`.
    pub host: String,
    /// Repository path, e.g. `user/repo`.
    pub path: String,
    /// Git ref (branch, tag, commit), when specified.
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// True when a ref was specified.
    pub pinned: bool,
}

/// Result of splitting an optional `@ref` suffix from a git URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitRef {
    pub repo: String,
    pub git_ref: Option<String>,
}

/// Hosted-git-info subset: domain, user, project and `#committish`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HostedInfo {
    domain: String,
    user: String,
    project: String,
    committish: Option<String>,
}

/// Split a trailing `@ref` (or `#ref` handled by callers) from `url`.
///
/// Three forms are recognized, matching upstream exactly:
/// - SCP-like `git@host:path`
/// - URLs containing `://` (rebuilt through `Url` so the ref is removed from the path)
/// - shorthand `host/path`
pub fn split_ref(url: &str) -> SplitRef {
    if let Some(rest) = url.strip_prefix("git@")
        && let Some((host, path_with_maybe_ref)) = rest.split_once(':')
    {
        if path_with_maybe_ref.is_empty() {
            return SplitRef {
                repo: url.to_string(),
                git_ref: None,
            };
        }
        return split_at_sign_in_path(
            path_with_maybe_ref,
            |repo_path| format!("git@{host}:{repo_path}"),
            url,
        );
    }

    if url.contains("://") {
        let Ok(mut parsed) = Url::parse(url) else {
            return SplitRef {
                repo: url.to_string(),
                git_ref: None,
            };
        };
        let path_with_maybe_ref = parsed.path().trim_start_matches('/');
        if !path_with_maybe_ref.contains('@') {
            return SplitRef {
                repo: url.to_string(),
                git_ref: None,
            };
        }
        let Some((repo_path, git_ref)) = split_once_into_nonempty(path_with_maybe_ref) else {
            return SplitRef {
                repo: url.to_string(),
                git_ref: None,
            };
        };
        let repo_path = repo_path.to_string();
        let git_ref = git_ref.to_string();
        parsed.set_path(&format!("/{repo_path}"));
        return SplitRef {
            repo: parsed.as_str().trim_end_matches('/').to_string(),
            git_ref: Some(git_ref),
        };
    }

    let Some(slash_index) = url.find('/') else {
        return SplitRef {
            repo: url.to_string(),
            git_ref: None,
        };
    };
    let host = &url[..slash_index];
    let path_with_maybe_ref = &url[slash_index + 1..];
    split_at_sign_in_path(
        path_with_maybe_ref,
        |repo_path| format!("{host}/{repo_path}"),
        url,
    )
}

fn split_at_sign_in_path<F>(path_with_maybe_ref: &str, build_repo: F, original: &str) -> SplitRef
where
    F: FnOnce(&str) -> String,
{
    let Some((repo_path, git_ref)) = split_once_into_nonempty(path_with_maybe_ref) else {
        return SplitRef {
            repo: original.to_string(),
            git_ref: None,
        };
    };
    SplitRef {
        repo: build_repo(repo_path),
        git_ref: Some(git_ref.to_string()),
    }
}

/// Split at the first `@`, but only when both halves are non-empty.
fn split_once_into_nonempty(value: &str) -> Option<(&str, &str)> {
    let (left, right) = value.split_once('@')?;
    if left.is_empty() || right.is_empty() {
        None
    } else {
        Some((left, right))
    }
}

/// Percent-decode a string the way `decodeURIComponent` does, returning `None`
/// for malformed escapes or invalid UTF-8.
fn decode_for_validation(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = hex_digit(bytes.get(i + 1).copied()?)?;
            let lo = hex_digit(bytes.get(i + 2).copied()?)?;
            decoded.push((hi << 4) | lo);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Reject install parts containing NUL, backslashes, a leading slash, `/..` segments,
/// or (when `allow_slash` is false) any slash. Both raw and percent-decoded forms
/// are checked, matching `hasUnsafeGitInstallPart`.
pub fn has_unsafe_git_install_part(value: &str, allow_slash: bool) -> bool {
    let decoded = match decode_for_validation(value) {
        Some(decoded) => decoded,
        None => return true,
    };
    for candidate in [value, decoded.as_str()] {
        let bytes = candidate.as_bytes();
        if bytes.contains(&0) || bytes.contains(&b'\\') || bytes.starts_with(b"/") {
            return true;
        }
        if !allow_slash && bytes.contains(&b'/') {
            return true;
        }
        if candidate.split('/').any(|segment| segment == "..") {
            return true;
        }
    }
    false
}

/// Validate host/path/repo and construct a [`GitSource`].
pub fn build_git_source(
    repo: String,
    host: String,
    path: String,
    git_ref: Option<String>,
) -> Option<GitSource> {
    if path.starts_with('/') {
        return None;
    }
    let without_git_suffix = path.strip_suffix(".git").unwrap_or(&path);
    let normalized_path = without_git_suffix.trim_start_matches('/');
    if host.is_empty() || normalized_path.is_empty() || normalized_path.split('/').count() < 2 {
        return None;
    }
    if has_unsafe_git_install_part(&host, false)
        || has_unsafe_git_install_part(normalized_path, true)
    {
        return None;
    }
    Some(GitSource {
        source_type: "git".to_string(),
        repo,
        host,
        path: normalized_path.to_string(),
        pinned: git_ref.is_some(),
        git_ref,
    })
}

fn is_known_host(host: &str) -> bool {
    matches!(
        host.to_ascii_lowercase().as_str(),
        "github.com" | "gitlab.com" | "bitbucket.org" | "gist.github.com"
    )
}

/// Minimal `hosted-git-info.fromUrl` equivalent for the default known hosts.
fn hosted_git_info(candidate: &str) -> Option<HostedInfo> {
    let (mut main, committish) = match candidate.split_once('#') {
        Some((main, committish)) if !committish.is_empty() => (main, Some(committish.to_string())),
        _ => (candidate, None),
    };
    main = main.trim_end_matches('/');
    main = main.strip_suffix(".git").unwrap_or(main);
    main = main.trim_end_matches('/');

    let (host, path) = if let Some(rest) = main.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        (host, path)
    } else if let Some(scheme_end) = main.find("://") {
        let mut rest = &main[scheme_end + 3..];
        // Strip credentials: everything before the last `@` in the authority part.
        if let Some(authority_end) = rest.find('/')
            && let Some(at_index) = rest[..authority_end].rfind('@')
        {
            rest = &rest[at_index + 1..];
        }
        match rest.find('/') {
            Some(slash) => (&rest[..slash], &rest[slash + 1..]),
            None => (rest, ""),
        }
    } else {
        let (host, path) = main.split_once('/')?;
        (host, path)
    };

    if !is_known_host(host) {
        return None;
    }
    let mut segments = path.split('/').filter(|segment| !segment.is_empty());
    let user = segments.next()?.to_string();
    let project_parts: Vec<&str> = segments.collect();
    if project_parts.is_empty() {
        return None;
    }
    Some(HostedInfo {
        domain: host.to_ascii_lowercase(),
        user,
        project: project_parts.join("/"),
        committish,
    })
}

fn is_explicit_protocol(value: &str) -> bool {
    ["http://", "https://", "ssh://", "git://"]
        .iter()
        .any(|prefix| value.to_ascii_lowercase().starts_with(prefix))
}

/// Generic fallback parser: SCP-like, explicit protocol URLs, or `host/path`
/// shorthand whose host contains a dot or equals `localhost`.
fn parse_generic_git_url(url: &str) -> Option<GitSource> {
    let SplitRef {
        repo: repo_without_ref,
        git_ref,
    } = split_ref(url);

    if let Some(rest) = repo_without_ref.strip_prefix("git@") {
        if let Some((host, path)) = rest.split_once(':') {
            if path.is_empty() {
                return None;
            }
            return build_git_source(
                repo_without_ref.clone(),
                host.to_string(),
                path.to_string(),
                git_ref,
            );
        }
        return None;
    }

    if is_explicit_protocol(&repo_without_ref) {
        let parsed = Url::parse(&repo_without_ref).ok()?;
        let host = parsed.host_str()?.to_string();
        let path = parsed.path().trim_start_matches('/').to_string();
        return build_git_source(repo_without_ref, host, path, git_ref);
    }

    let (host, path) = repo_without_ref.split_once('/')?;
    if !host.contains('.') && host != "localhost" {
        return None;
    }
    let repo = format!("https://{repo_without_ref}");
    build_git_source(repo, host.to_string(), path.to_string(), git_ref)
}

/// Parse a git source into a [`GitSource`].
///
/// With the `git:` prefix all historical shorthand forms are accepted; without
/// it, only explicit `http(s)://`, `ssh://` and `git://` URLs are accepted.
pub fn parse_git_url(source: &str) -> Option<GitSource> {
    let trimmed = source.trim();
    let has_git_prefix = trimmed.starts_with("git:");
    let url = if has_git_prefix {
        trimmed[4..].trim()
    } else {
        trimmed
    };

    if !has_git_prefix && !is_explicit_protocol(url) {
        return None;
    }

    let SplitRef {
        repo: split_repo,
        git_ref,
    } = split_ref(url);

    let mut hosted_candidates = Vec::new();
    if let Some(git_ref) = &git_ref {
        hosted_candidates.push(format!("{split_repo}#{git_ref}"));
    }
    hosted_candidates.push(url.to_string());

    for candidate in &hosted_candidates {
        let Some(info) = hosted_git_info(candidate) else {
            continue;
        };
        if git_ref.is_some() && info.project.contains('@') {
            continue;
        }
        let use_https_prefix = !["http://", "https://", "ssh://", "git://", "git@"]
            .iter()
            .any(|prefix| split_repo.starts_with(prefix));
        let repo = if use_https_prefix {
            format!("https://{split_repo}")
        } else {
            split_repo.clone()
        };
        let resolved_ref = info.committish.or_else(|| git_ref.clone());
        return build_git_source(
            repo,
            info.domain,
            format!("{}/{}", info.user, info.project),
            resolved_ref,
        );
    }

    let mut https_candidates = Vec::new();
    if let Some(git_ref) = &git_ref {
        https_candidates.push(format!("https://{split_repo}#{git_ref}"));
    }
    https_candidates.push(format!("https://{url}"));

    for candidate in &https_candidates {
        let Some(info) = hosted_git_info(candidate) else {
            continue;
        };
        if git_ref.is_some() && info.project.contains('@') {
            continue;
        }
        let resolved_ref = info.committish.or_else(|| git_ref.clone());
        return build_git_source(
            format!("https://{split_repo}"),
            info.domain,
            format!("{}/{}", info.user, info.project),
            resolved_ref,
        );
    }

    parse_generic_git_url(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(repo: &str, host: &str, path: &str, git_ref: Option<&str>) -> GitSource {
        GitSource {
            source_type: "git".to_string(),
            repo: repo.to_string(),
            host: host.to_string(),
            path: path.to_string(),
            git_ref: git_ref.map(str::to_string),
            pinned: git_ref.is_some(),
        }
    }

    #[test]
    fn parses_https_url() {
        assert_eq!(
            parse_git_url("https://github.com/user/repo"),
            Some(source(
                "https://github.com/user/repo",
                "github.com",
                "user/repo",
                None
            ))
        );
    }

    #[test]
    fn parses_ssh_protocol_url() {
        assert_eq!(
            parse_git_url("ssh://git@github.com/user/repo"),
            Some(source(
                "ssh://git@github.com/user/repo",
                "github.com",
                "user/repo",
                None
            ))
        );
    }

    #[test]
    fn parses_protocol_url_with_ref() {
        assert_eq!(
            parse_git_url("https://github.com/user/repo@v1.0.0"),
            Some(source(
                "https://github.com/user/repo",
                "github.com",
                "user/repo",
                Some("v1.0.0")
            ))
        );
    }

    #[test]
    fn parses_scp_like_url_with_git_prefix() {
        assert_eq!(
            parse_git_url("git:git@github.com:user/repo"),
            Some(source(
                "git@github.com:user/repo",
                "github.com",
                "user/repo",
                None
            ))
        );
    }

    #[test]
    fn parses_host_path_shorthand_with_git_prefix() {
        assert_eq!(
            parse_git_url("git:github.com/user/repo"),
            Some(source(
                "https://github.com/user/repo",
                "github.com",
                "user/repo",
                None
            ))
        );
    }

    #[test]
    fn parses_shorthand_with_ref_and_git_prefix() {
        assert_eq!(
            parse_git_url("git:git@github.com:user/repo@v1.0.0"),
            Some(source(
                "git@github.com:user/repo",
                "github.com",
                "user/repo",
                Some("v1.0.0")
            ))
        );
    }

    #[test]
    fn rejects_unsafe_git_install_paths() {
        for unsafe_source in [
            "git:git@evil.example:../../victim/repo",
            "https://evil.example/..%2F..%2Fvictim/repo",
            "https://evil.example/..%2F..%2Fvictim/repo%",
            "git:git@evil.example:/absolute/repo",
            "git:git@evil.example:user\\repo/name",
            "git:git@evil.example:user/repo\0name",
        ] {
            assert_eq!(
                parse_git_url(unsafe_source),
                None,
                "should reject {unsafe_source}"
            );
        }
    }

    #[test]
    fn rejects_unsupported_without_git_prefix() {
        for source in [
            "git@github.com:user/repo",
            "github.com/user/repo",
            "user/repo",
        ] {
            assert_eq!(parse_git_url(source), None, "should reject {source}");
        }
    }
}
