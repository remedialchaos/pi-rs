//! A session's files in its worker's `Files`: the spaces p2 writes (uploads, downloads), whole reads
//! of a version, chunked writes from a reader, and the download snapshots producers (`send_file`) save.
use super::files::{DownloadInfo, safe_file_name};
pub use pi_rs_chord::node::app::{FileInfo, FileWrite, Files, FilesFolder, SpaceSummary};

/// The space of a session's uploads (`POST /upload`): path `<id>-<name>`, key = the upload's id.
pub const UPLOADS: &str = "uploads";

/// The space of a session's download snapshots (`send_file`): path `<id>-<name>`, key = the download's id.
pub const DOWNLOADS: &str = "downloads";

/// The largest download snapshot: 25 MiB.
pub const MAX_DOWNLOAD_BYTES: u64 = 25 * 1024 * 1024;

/// The largest piece `Files` takes or gives in one call.
pub const FILE_CHUNK: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("File exceeds {0} MiB")]
    TooLarge(f64),
    #[error("File {0} changed while read")]
    Changed(String),
    #[error(transparent)]
    Store(#[from] pi_rs_chord::Error),
    #[error(transparent)]
    Reader(#[from] std::io::Error),
}
impl DownloadError {
    /// The HTTP status this error answers with (`TooLarge` is a 413; the rest are internal).
    pub fn status(&self) -> Option<u16> {
        matches!(self, Self::TooLarge(_)).then_some(413)
    }
}

/// The whole bytes of version `file` (read a chunk at a time).
pub fn read_whole(files: &Files, file: &FileInfo) -> Result<Vec<u8>, DownloadError> {
    let mut out = Vec::new();
    while (out.len() as u64) < file.size {
        let bytes = files.read(&file.id, out.len() as u64, FILE_CHUNK)?;
        if bytes.is_empty() {
            return Err(DownloadError::Changed(file.path.clone()));
        }
        out.extend(bytes);
    }
    Ok(out)
}

/// Writes the bytes `next` gives (until it gives none) as a new version, at most `max` bytes
/// (`TooLarge` past it); an upload that fails is dropped.
pub async fn write_chunks<F: Future<Output = std::io::Result<Vec<u8>>>>(
    files: &Files,
    write: FileWrite,
    mut next: impl FnMut() -> F,
    max: u64,
) -> Result<FileInfo, DownloadError> {
    if let Some(existing) = write
        .key
        .as_ref()
        .and_then(|key| files.by_key(&write.space, key))
    {
        return Ok(existing);
    }
    let upload = files.begin(write)?;
    let result = async {
        let mut offset = 0;
        loop {
            let bytes = next().await?;
            if bytes.is_empty() {
                break;
            }
            if bytes.len() as u64 > max - offset {
                return Err(DownloadError::TooLarge(max as f64 / 1048576.0));
            }
            for chunk in bytes.chunks(FILE_CHUNK) {
                offset = files.append(&upload, offset, chunk).await?;
            }
        }
        Ok(files.finish(&upload).await?)
    }
    .await;
    if result.is_err() {
        // The `.catch(() => {})` of upstream: cleanup failures must not mask the real error.
        let _ = files.cancel(&upload);
    }
    result
}

/// Saves a download snapshot of `name` (a display name, made safe) from the bytes `next` gives,
/// written by `source`: capped at 25 MiB even if the source grows meanwhile. Its id (a UUID) is the
/// version's key, what `/download` names.
pub async fn save_download<F: Future<Output = std::io::Result<Vec<u8>>>>(
    files: &Files,
    name: &str,
    size: u64,
    source: &str,
    reference: Option<String>,
    next: impl FnMut() -> F,
) -> Result<DownloadInfo, DownloadError> {
    if size > MAX_DOWNLOAD_BYTES {
        return Err(DownloadError::TooLarge(25.0));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let name = safe_file_name(name);
    let mut write = FileWrite::new(DOWNLOADS, &format!("{id}-{name}"));
    write.source = source.into();
    write.reference = reference;
    write.key = Some(id.clone());
    write.meta = Some(serde_json::json!({ "name": name }));
    let file = write_chunks(files, write, next, MAX_DOWNLOAD_BYTES).await?;
    Ok(DownloadInfo {
        id,
        name,
        size: file.size,
    })
}

/// The download snapshot `id` of the session (`None` for anything else) and its display name.
pub fn find_download(files: &Files, id: &str) -> Option<(FileInfo, String)> {
    if id.is_empty() {
        return None;
    }
    let file = files.by_key(DOWNLOADS, id)?;
    let name = display_name(&file).to_owned();
    Some((file, name))
}

/// A file's display name: `meta.name` as p2 writes it, else its path's last segment.
pub fn display_name(file: &FileInfo) -> &str {
    file.meta
        .as_ref()
        .and_then(|m| m.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or_else(|| file.path.rsplit('/').next().unwrap_or(&file.path))
}
