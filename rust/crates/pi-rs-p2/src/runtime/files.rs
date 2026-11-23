use serde::Serialize;
use unicode_normalization::UnicodeNormalization;

pub fn safe_file_name(name: &str) -> String {
    let mut safe = String::new();
    let mut replacing = false;
    for c in name.rsplit(['/', '\\']).next().unwrap_or_default().nfkc() {
        let valid = c.is_numeric() || c.is_alphabetic() || matches!(c, '.' | '_' | '-');
        if valid {
            safe.push(c);
        } else if !replacing {
            safe.push('_');
        }
        replacing = !valid;
    }
    let units: Vec<_> = safe.trim_start_matches('.').encode_utf16().collect();
    if units.is_empty() {
        return "file".into();
    }
    // JavaScript slice can retain a lone low surrogate. Rust strings encode it as U+FFFD.
    String::from_utf16_lossy(&units[units.len().saturating_sub(120)..])
}

#[derive(Debug, Serialize, PartialEq)]
pub struct DownloadInfo {
    pub id: String,
    pub name: String,
    pub size: u64,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FilePreview {
    pub kind: &'static str,
    pub mime_type: &'static str,
}

pub fn file_preview(name: &str) -> Option<FilePreview> {
    let (kind, mime_type) = match name.rsplit('.').next()?.to_ascii_lowercase().as_str() {
        "txt" | "md" | "markdown" | "js" | "jsx" | "ts" | "tsx" | "json" | "css" | "html"
        | "htm" | "xml" | "yaml" | "yml" | "sh" | "py" | "rs" | "go" | "java" | "c" | "h"
        | "cpp" | "sql" | "log" => ("text", "text/plain; charset=utf-8"),
        "png" => ("image", "image/png"),
        "jpg" | "jpeg" => ("image", "image/jpeg"),
        "gif" => ("image", "image/gif"),
        "webp" => ("image", "image/webp"),
        "avif" => ("image", "image/avif"),
        "mp3" => ("audio", "audio/mpeg"),
        "m4a" => ("audio", "audio/mp4"),
        "aac" => ("audio", "audio/aac"),
        "wav" => ("audio", "audio/wav"),
        "oga" | "ogg" => ("audio", "audio/ogg"),
        "flac" => ("audio", "audio/flac"),
        "mp4" | "m4v" => ("video", "video/mp4"),
        "webm" => ("video", "video/webm"),
        "ogv" => ("video", "video/ogg"),
        "mov" => ("video", "video/quicktime"),
        _ => return None,
    };
    Some(FilePreview { kind, mime_type })
}
