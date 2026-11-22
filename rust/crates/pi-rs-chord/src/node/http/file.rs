use super::{HttpError, HttpRequest, HttpResponse, HttpResult, text_response};
use axum::body::Body;
use http::{HeaderValue, StatusCode};
use std::{
    path::{Component, Path},
    time::UNIX_EPOCH,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

#[derive(Clone, Debug)]
pub struct FileOptions {
    pub ranges: bool,
    pub mime: Option<String>,
    pub download: Option<String>,
    pub inline: bool,
}
impl Default for FileOptions {
    fn default() -> Self {
        Self {
            ranges: true,
            mime: None,
            download: None,
            inline: false,
        }
    }
}

/// Resolve a client path below a real root, refusing dot segments and links out.
pub async fn contained(root: &Path, relative: &str) -> HttpResult<std::path::PathBuf> {
    let path = Path::new(relative);
    if path
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
        || relative.contains('\\')
        || relative.contains('\0')
        || relative
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return Err(HttpError::new(403, "the file lies outside its root"));
    }
    let base = tokio::fs::canonicalize(root).await?;
    let real = tokio::fs::canonicalize(base.join(path)).await?;
    if !real.starts_with(&base) {
        return Err(HttpError::new(403, "the file lies outside its root"));
    }
    Ok(real)
}

pub async fn file_response(
    root: &Path,
    relative: &str,
    request: &HttpRequest,
    options: FileOptions,
) -> HttpResult<HttpResponse> {
    let path = contained(root, relative).await?;
    let metadata = tokio::fs::metadata(&path).await?;
    if !metadata.is_file() {
        return Err(HttpError::new(404, "no such file"));
    }
    let size = metadata.len();
    let stamp = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let etag = format!("\"{size:x}-{stamp:x}\"");
    if request.header("if-none-match") == Some(etag.as_str()) {
        let mut response = text_response(304, "")?;
        response
            .headers_mut()
            .insert("etag", HeaderValue::from_str(&etag)?);
        return Ok(response);
    }
    let mut start = 0;
    let mut length = size;
    let mut status = StatusCode::OK;
    if options.ranges && request.header("if-range").is_none_or(|value| value == etag) {
        match range_of(request.header("range"), size) {
            Range::Unsatisfiable => {
                let mut response = text_response(416, "")?;
                response.headers_mut().insert(
                    "content-range",
                    HeaderValue::from_str(&format!("bytes */{size}"))?,
                );
                return Ok(response);
            }
            Range::Bytes(offset, count) => {
                start = offset;
                length = count;
                status = StatusCode::PARTIAL_CONTENT;
            }
            Range::Whole => {}
        }
    }
    let body = if request.method == http::Method::HEAD || length == 0 {
        Body::empty()
    } else {
        let stream = futures_util::stream::try_unfold(
            (None::<tokio::fs::File>, path.clone(), start, length),
            |(file, path, offset, remaining)| async move {
                if remaining == 0 {
                    return Ok::<_, std::io::Error>(None);
                }
                let mut file = match file {
                    Some(file) => file,
                    None => {
                        let mut file = tokio::fs::File::open(&path).await?;
                        file.seek(std::io::SeekFrom::Start(offset)).await?;
                        file
                    }
                };
                let mut bytes = vec![0; remaining.min(256 * 1024) as usize];
                file.read_exact(&mut bytes).await?;
                let read = bytes.len() as u64;
                Ok(Some((
                    bytes,
                    (Some(file), path, offset + read, remaining - read),
                )))
            },
        );
        Body::from_stream(stream)
    };
    let mut response = HttpResponse::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert("etag", HeaderValue::from_str(&etag)?);
    headers.insert(
        "content-length",
        HeaderValue::from_str(&length.to_string())?,
    );
    headers.insert(
        "content-type",
        HeaderValue::from_str(options.mime.as_deref().unwrap_or_else(|| mime_of(&path)))?,
    );
    let disposition = if options.inline {
        "inline"
    } else {
        "attachment"
    };
    let disposition = options.download.as_ref().map(|name| {
        let clean: String = name.chars().filter(|c| !c.is_control() && !matches!(*c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')).collect();
        let ascii: String = clean.chars().map(|c| if c.is_ascii() && c != '"' && c != '\\' { c } else { '_' }).collect();
        format!("{disposition}; filename=\"{ascii}\"; filename*=UTF-8''{}", super::percent_encode(&clean).replace('\'', "%27").replace('(', "%28").replace(')', "%29").replace('*', "%2A"))
    }).unwrap_or_else(|| disposition.into());
    headers.insert("content-disposition", HeaderValue::from_str(&disposition)?);
    if options.inline {
        headers.insert(
            "content-security-policy",
            HeaderValue::from_static("default-src 'none'; sandbox"),
        );
    }
    if options.ranges {
        headers.insert("accept-ranges", HeaderValue::from_static("bytes"));
    }
    if status == StatusCode::PARTIAL_CONTENT {
        headers.insert(
            "content-range",
            HeaderValue::from_str(&format!("bytes {start}-{}/{size}", start + length - 1))?,
        );
    }
    Ok(response)
}

enum Range {
    Whole,
    Unsatisfiable,
    Bytes(u64, u64),
}
fn range_of(header: Option<&str>, size: u64) -> Range {
    let Some((from, to)) = header
        .and_then(|h| h.trim().strip_prefix("bytes="))
        .and_then(|h| h.split_once('-'))
    else {
        return Range::Whole;
    };
    if !from.bytes().chain(to.bytes()).all(|b| b.is_ascii_digit())
        || (from.is_empty() && to.is_empty())
    {
        return Range::Whole;
    }
    if from.is_empty() {
        let suffix = to.parse::<u64>().unwrap_or(u64::MAX);
        let length = suffix.min(size);
        return if length == 0 {
            Range::Unsatisfiable
        } else {
            Range::Bytes(size - length, length)
        };
    }
    let start = from.parse::<u64>().unwrap_or(u64::MAX);
    if start >= size {
        return Range::Unsatisfiable;
    }
    let end = if to.is_empty() {
        size - 1
    } else {
        to.parse::<u64>().unwrap_or(u64::MAX).min(size - 1)
    };
    if end < start {
        Range::Whole
    } else {
        Range::Bytes(start, end - start + 1)
    }
}
pub fn mime_of(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("txt" | "log") => "text/plain; charset=utf-8",
        Some("md") => "text/markdown; charset=utf-8",
        Some("csv") => "text/csv; charset=utf-8",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("mp3") => "audio/mpeg",
        Some("m4a") => "audio/mp4",
        Some("ogg" | "opus") => "audio/ogg",
        Some("wav") => "audio/wav",
        Some("flac") => "audio/flac",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mov") => "video/quicktime",
        _ => "application/octet-stream",
    }
}
