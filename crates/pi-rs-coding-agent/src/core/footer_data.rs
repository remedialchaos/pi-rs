//! Footer data and formatting. Ports the git metadata half of
//! `core/footer-data-provider.ts` and adds a pure snapshot -> string formatter so
//! the model, thinking level, tool count, session name and token count can be
//! rendered without a UI.
//!
//! No watchers or timers are owned here: callers sample a [`FooterSnapshot`] when
//! they repaint, which keeps the module deterministic and offline-testable.

use std::path::{Path, PathBuf};

use indexmap::IndexMap;

/// Git metadata paths found by walking up from a working directory.
#[derive(Clone, Debug, PartialEq)]
pub struct GitPaths {
    pub repo_dir: PathBuf,
    pub common_git_dir: PathBuf,
    pub head_path: PathBuf,
}

fn absolute(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// Find git metadata paths, handling both normal repos and worktrees whose
/// `.git` is a file pointing at a gitdir.
pub fn find_git_paths(cwd: &Path) -> Option<GitPaths> {
    let mut dir = cwd.to_path_buf();
    loop {
        let git_path = dir.join(".git");
        if git_path.exists() {
            let Ok(metadata) = std::fs::metadata(&git_path) else {
                return None;
            };
            if metadata.is_file() {
                let Ok(content) = std::fs::read_to_string(&git_path) else {
                    return None;
                };
                let content = content.trim();
                let relative = content.strip_prefix("gitdir: ")?;
                let git_dir = absolute(&dir, Path::new(relative.trim()));
                let head_path = git_dir.join("HEAD");
                if !head_path.exists() {
                    return None;
                }
                let common_file = git_dir.join("commondir");
                let common_git_dir = if common_file.exists() {
                    std::fs::read_to_string(&common_file).map_or_else(
                        |_| git_dir.clone(),
                        |text| absolute(&git_dir, Path::new(text.trim())),
                    )
                } else {
                    git_dir
                };
                return Some(GitPaths {
                    repo_dir: dir,
                    common_git_dir,
                    head_path,
                });
            }
            if metadata.is_dir() {
                let head_path = git_path.join("HEAD");
                if !head_path.exists() {
                    return None;
                }
                return Some(GitPaths {
                    repo_dir: dir,
                    common_git_dir: git_path,
                    head_path,
                });
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn branch_via_git(repo_dir: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args([
            "--no-optional-locks",
            "symbolic-ref",
            "--quiet",
            "--short",
            "HEAD",
        ])
        .current_dir(repo_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() {
        None
    } else {
        Some(branch)
    }
}

/// Read the current branch from HEAD: a branch name, `"detached"`, or `None`
/// when there is no git repository.
pub fn resolve_git_branch(cwd: &Path) -> Option<String> {
    let paths = find_git_paths(cwd)?;
    let content = std::fs::read_to_string(&paths.head_path).ok()?;
    let content = content.trim();
    let Some(branch) = content.strip_prefix("ref: refs/heads/") else {
        return Some("detached".to_string());
    };
    if branch == ".invalid" {
        return Some(branch_via_git(&paths.repo_dir).unwrap_or_else(|| "detached".to_string()));
    }
    Some(branch.to_string())
}

/// Immutable input to the footer formatter.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FooterSnapshot {
    pub model_id: Option<String>,
    pub provider: Option<String>,
    pub thinking_level: Option<String>,
    pub tool_count: Option<usize>,
    pub session_name: Option<String>,
    pub tokens: Option<u64>,
    pub git_branch: Option<String>,
    pub extension_statuses: IndexMap<String, String>,
    pub available_provider_count: usize,
}

impl FooterSnapshot {
    /// Model segment: `provider/id` when both are present.
    pub fn model_segment(&self) -> Option<String> {
        match (&self.provider, &self.model_id) {
            (Some(provider), Some(id)) if !provider.is_empty() && !id.is_empty() => {
                Some(format!("{provider}/{id}"))
            }
            (Some(provider), None) if !provider.is_empty() => Some(provider.clone()),
            (None, Some(id)) if !id.is_empty() => Some(id.clone()),
            _ => None,
        }
    }

    /// All non-empty footer segments in a stable order.
    pub fn segments(&self) -> Vec<String> {
        let mut segments = Vec::new();
        if let Some(model) = self.model_segment() {
            segments.push(model);
        }
        if let Some(thinking) = &self.thinking_level
            && !thinking.is_empty()
        {
            segments.push(format!("thinking: {thinking}"));
        }
        if let Some(count) = self.tool_count {
            segments.push(format!("{count} tools"));
        }
        if let Some(name) = &self.session_name
            && !name.is_empty()
        {
            segments.push(format!("session: {name}"));
        }
        if let Some(tokens) = self.tokens {
            segments.push(format!("{} tokens", format_token_count(tokens)));
        }
        if let Some(branch) = &self.git_branch
            && !branch.is_empty()
        {
            segments.push(format!("branch: {branch}"));
        }
        if self.available_provider_count > 0 {
            segments.push(format!("{} providers", self.available_provider_count));
        }
        let mut statuses: Vec<(&String, &String)> = self
            .extension_statuses
            .iter()
            .filter(|(_, value)| !value.is_empty())
            .collect();
        statuses.sort_by(|left, right| left.0.cmp(right.0));
        for (name, value) in statuses {
            segments.push(format!("{name}: {value}"));
        }
        segments
    }
}

/// Render a snapshot as one footer line. Segments are separated by `" | "`.
pub fn build_footer(snapshot: &FooterSnapshot) -> String {
    snapshot.segments().join(" | ")
}

/// Group digits with commas, matching the upstream token display.
pub fn format_token_count(tokens: u64) -> String {
    let digits = tokens.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Live provider of footer data. Only state that affects formatting is kept.
#[derive(Clone, Debug)]
pub struct FooterDataProvider {
    cwd: PathBuf,
    extension_statuses: IndexMap<String, String>,
    available_provider_count: usize,
}

impl FooterDataProvider {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            extension_statuses: IndexMap::new(),
            available_provider_count: 0,
        }
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Current git branch, `"detached"` for detached HEAD, `None` outside a repo.
    pub fn get_git_branch(&self) -> Option<String> {
        resolve_git_branch(&self.cwd)
    }

    /// Extension status texts set through `ctx.ui.setStatus()`.
    pub fn get_extension_statuses(&self) -> &IndexMap<String, String> {
        &self.extension_statuses
    }

    pub fn set_extension_status(&mut self, key: impl Into<String>, text: Option<&str>) {
        let key = key.into();
        match text {
            Some(text) => {
                self.extension_statuses.insert(key, text.to_string());
            }
            None => {
                self.extension_statuses.shift_remove(&key);
            }
        }
    }

    pub fn clear_extension_statuses(&mut self) {
        self.extension_statuses.clear();
    }

    pub fn get_available_provider_count(&self) -> usize {
        self.available_provider_count
    }

    pub fn set_available_provider_count(&mut self, count: usize) {
        self.available_provider_count = count;
    }

    pub fn set_cwd(&mut self, cwd: impl Into<PathBuf>) {
        self.cwd = cwd.into();
    }

    /// Snapshot the data needed for one footer repaint.
    pub fn snapshot(&self) -> FooterSnapshot {
        FooterSnapshot {
            git_branch: self.get_git_branch(),
            extension_statuses: self.extension_statuses.clone(),
            available_provider_count: self.available_provider_count,
            ..FooterSnapshot::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(dir: &Path, head: &str) -> Result<(), String> {
        std::fs::create_dir_all(dir.join(".git")).map_err(|e| e.to_string())?;
        std::fs::write(dir.join(".git/HEAD"), head).map_err(|e| e.to_string())
    }

    #[test]
    fn finds_git_paths_from_nested_directory() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        repo(dir.path(), "ref: refs/heads/main\n")?;
        let nested = dir.path().join("a/b");
        std::fs::create_dir_all(&nested).map_err(|e| e.to_string())?;
        let paths = find_git_paths(&nested).ok_or("no git paths")?;
        assert_eq!(paths.repo_dir, dir.path());
        assert_eq!(paths.head_path, dir.path().join(".git/HEAD"));
        assert_eq!(paths.common_git_dir, dir.path().join(".git"));
        Ok(())
    }

    #[test]
    fn reads_branch_and_detached_head() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        repo(dir.path(), "ref: refs/heads/feature/foo\n")?;
        assert_eq!(
            resolve_git_branch(dir.path()).as_deref(),
            Some("feature/foo")
        );
        std::fs::write(dir.path().join(".git/HEAD"), "0123456789abcdef\n")
            .map_err(|e| e.to_string())?;
        assert_eq!(resolve_git_branch(dir.path()).as_deref(), Some("detached"));
        Ok(())
    }

    #[test]
    fn worktree_git_file_is_followed() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let git_dir = dir.path().join("real-git");
        std::fs::create_dir_all(&git_dir).map_err(|e| e.to_string())?;
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/work\n")
            .map_err(|e| e.to_string())?;
        std::fs::write(dir.path().join(".git"), "gitdir: real-git\n").map_err(|e| e.to_string())?;
        assert_eq!(resolve_git_branch(dir.path()).as_deref(), Some("work"));
        Ok(())
    }

    #[test]
    fn no_repository_returns_none() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        assert_eq!(resolve_git_branch(dir.path()), None);
        Ok(())
    }

    #[test]
    fn token_counts_are_grouped() {
        assert_eq!(format_token_count(1), "1");
        assert_eq!(format_token_count(999), "999");
        assert_eq!(format_token_count(1_000), "1,000");
        assert_eq!(format_token_count(12_345_678), "12,345,678");
    }

    #[test]
    fn footer_includes_model_thinking_tools_session_and_tokens() {
        let footer = build_footer(&FooterSnapshot {
            model_id: Some("claude-sonnet-4-5".into()),
            provider: Some("anthropic".into()),
            thinking_level: Some("high".into()),
            tool_count: Some(4),
            session_name: Some("demo".into()),
            tokens: Some(12_300),
            ..FooterSnapshot::default()
        });
        assert_eq!(
            footer,
            "anthropic/claude-sonnet-4-5 | thinking: high | 4 tools | session: demo | 12,300 tokens"
        );
    }

    #[test]
    fn footer_omits_empty_segments_and_sorts_statuses() {
        let mut statuses = IndexMap::new();
        statuses.insert("zeta".to_string(), "z".to_string());
        statuses.insert("alpha".to_string(), "a".to_string());
        statuses.insert("blank".to_string(), String::new());
        let footer = build_footer(&FooterSnapshot {
            provider: Some("openai".into()),
            model_id: None,
            git_branch: Some("main".into()),
            available_provider_count: 2,
            extension_statuses: statuses,
            ..FooterSnapshot::default()
        });
        assert_eq!(
            footer,
            "openai | branch: main | 2 providers | alpha: a | zeta: z"
        );
    }

    #[test]
    fn provider_tracks_extension_statuses() {
        let mut provider = FooterDataProvider::new("/tmp/nonexistent-footer-dir");
        provider.set_extension_status("mcp", Some("connected"));
        assert_eq!(
            provider
                .get_extension_statuses()
                .get("mcp")
                .map(String::as_str),
            Some("connected")
        );
        provider.set_extension_status("mcp", None);
        assert!(provider.get_extension_statuses().is_empty());
        provider.set_available_provider_count(3);
        assert_eq!(provider.get_available_provider_count(), 3);
        assert_eq!(provider.snapshot().available_provider_count, 3);
    }
}
