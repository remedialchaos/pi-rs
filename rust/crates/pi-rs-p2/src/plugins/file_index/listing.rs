//! Environment-backed file listing for `file-index` (section 8.3).

use pi_rs_chord::context::Context;
use pi_rs_durable::env::{ExecutionEnv, FileError, FileErrorCode, FileKind, ListFilesOptions};

use super::rank::FileItem;

pub const LIST_ENTRIES: u64 = 50_000;
pub const LIST_BYTES: u64 = 4 * 1024 * 1024;
pub const LIST_TIMEOUT_MS: u64 = 10_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedIndex {
    pub items: Vec<FileItem>,
    pub truncated: bool,
}

/// Lists files and directories below `root` using `env.list_files`.
/// Returns `Ok(None)` when the environment does not support `list_files` (`NotSupported`).
pub async fn list_index(
    env: &dyn ExecutionEnv,
    root: &str,
    context: &Context,
) -> Result<Option<ListedIndex>, FileError> {
    let options = ListFilesOptions {
        exclude: vec!["node_modules".into()],
        max_entries: Some(LIST_ENTRIES),
        max_bytes: Some(LIST_BYTES),
        timeout_ms: Some(LIST_TIMEOUT_MS),
        ..Default::default()
    };
    match env.list_files(root, Some(options), context).await {
        Ok(listed) => Ok(Some(ListedIndex {
            items: listed
                .entries
                .into_iter()
                .map(|entry| FileItem {
                    path: entry.path,
                    dir: entry.kind == FileKind::Directory,
                })
                .collect(),
            truncated: listed.truncated,
        })),
        Err(error) if error.code == FileErrorCode::NotSupported => Ok(None),
        Err(error) => Err(error),
    }
}
