use super::files::{DownloadInfo, safe_file_name};
pub use pi_rs_chord::node::app::{FileInfo, FileWrite, Files, FilesFolder, SpaceSummary};
use std::future::Future;

pub const UPLOADS: &str = "uploads";
pub const DOWNLOADS: &str = "downloads";
pub const FILE_CHUNK: usize = 1024 * 1024;
pub const MAX_DOWNLOAD_BYTES: u64 = 25 * 1024 * 1024;

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
    pub fn status(&self) -> Option<u16> {
        matches!(self, Self::TooLarge(_)).then_some(413)
    }
}
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
        let _ = files.cancel(&upload);
    }
    result
}
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
    write.meta = Some(serde_json::json!({"name":name}));
    let file = write_chunks(files, write, next, MAX_DOWNLOAD_BYTES).await?;
    Ok(DownloadInfo {
        id,
        name,
        size: file.size,
    })
}
pub fn display_name(file: &FileInfo) -> &str {
    file.meta
        .as_ref()
        .and_then(|m| m.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or_else(|| file.path.rsplit('/').next().unwrap_or(&file.path))
}
pub fn find_download(files: &Files, id: &str) -> Option<(FileInfo, String)> {
    if id.is_empty() {
        return None;
    }
    let file = files.by_key(DOWNLOADS, id)?;
    let name = display_name(&file).to_owned();
    Some((file, name))
}
