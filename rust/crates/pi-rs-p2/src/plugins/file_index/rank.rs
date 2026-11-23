//! Ranking for `@` completion matching upstream `apps/p2/plugins/file-index/rank.ts`.

use std::collections::BTreeSet;

use pi_rs_tui::fuzzy::fuzzy_match;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileItem {
    pub path: String,
    pub dir: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RankKind {
    #[default]
    Any,
    File,
    Dir,
}

impl RankKind {
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("file") => Self::File,
            Some("dir") => Self::Dir,
            _ => Self::Any,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RankOptions {
    pub limit: usize,
    pub kind: RankKind,
}

impl Default for RankOptions {
    fn default() -> Self {
        Self {
            limit: 20,
            kind: RankKind::Any,
        }
    }
}

fn basename(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// Upstream `scoreEntry`: exact name 100, name prefix 80, name substring 50, path substring 30;
/// fuzzy match below every substring match in `[1, 20]`; directories +10 when score > 0.
pub fn score_entry(path: &str, query: &str, dir: bool) -> f64 {
    let name = basename(path).to_lowercase();
    let lower = query.to_lowercase();
    let mut score = if name == lower {
        100.0
    } else if name.starts_with(&lower) {
        80.0
    } else if name.contains(&lower) {
        50.0
    } else if path.to_lowercase().contains(&lower) {
        30.0
    } else {
        let name_fuzzy = fuzzy_match(&lower, &name);
        let fuzzy = if name_fuzzy.matches {
            name_fuzzy
        } else {
            fuzzy_match(&lower, path)
        };
        if fuzzy.matches {
            (20.0 - fuzzy.score / 10.0).clamp(1.0, 20.0)
        } else {
            0.0
        }
    };
    if dir && score > 0.0 {
        score += 10.0;
    }
    score
}

pub fn rank_files(entries: &[FileItem], query: &str, options: RankOptions) -> Vec<FileItem> {
    let text = query.strip_prefix("./").unwrap_or(query);
    let mut scored: Vec<(&FileItem, f64, usize)> = Vec::new();
    for item in entries {
        match options.kind {
            RankKind::File if item.dir => continue,
            RankKind::Dir if !item.dir => continue,
            _ => {}
        }
        let score = if text.is_empty() {
            1.0
        } else {
            score_entry(&item.path, text, item.dir)
        };
        if score > 0.0 {
            let depth = item.path.split('/').count();
            scored.push((item, score, depth));
        }
    }
    scored.sort_by(|a, b| {
        b.1.total_cmp(&a.1)
            .then_with(|| a.2.cmp(&b.2))
            .then_with(|| a.0.path.len().cmp(&b.0.path.len()))
            .then_with(|| a.0.path.cmp(&b.0.path))
    });
    scored
        .into_iter()
        .take(options.limit)
        .map(|(item, _, _)| item.clone())
        .collect()
}

/// Files plus the directories their paths imply.
pub fn with_directories(files: &[String]) -> Vec<FileItem> {
    let mut dirs = BTreeSet::new();
    for file in files {
        let mut search_from = 0;
        while let Some(offset) = file[search_from..].find('/') {
            let end = search_from + offset;
            dirs.insert(file[..end].to_owned());
            search_from = end + 1;
        }
    }
    let mut out = Vec::with_capacity(dirs.len() + files.len());
    for path in dirs {
        out.push(FileItem { path, dir: true });
    }
    for path in files {
        out.push(FileItem {
            path: path.clone(),
            dir: false,
        });
    }
    out
}
