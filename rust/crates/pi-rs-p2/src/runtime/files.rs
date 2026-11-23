//! A display filename, never a path or a raw HTTP header value.
use regex::Regex;
use serde::Serialize;
use std::sync::LazyLock;
use unicode_normalization::UnicodeNormalization;

static UNSAFE: LazyLock<Regex> = LazyLock::new(|| {
    // INFALLIBLE: This literal uses only the supported `\p{L}`/`\p{N}` categories and a plain class.
    Regex::new(r"[^\p{L}\p{N}._-]+").expect("static filename sanitizer pattern")
});
static LEADING_DOTS: LazyLock<Regex> = LazyLock::new(|| {
    // INFALLIBLE: A literal anchored class with no metasyntactic ambiguity.
    Regex::new(r"^\.+").expect("static leading-dot pattern")
});

/// A display filename, never a path or a raw HTTP header value.
///
/// Mirrors the upstream `safeFileName`: take the last `/`- or `\`-separated segment, NFKC-normalize
/// it, replace every run of characters outside letters/numbers/`._-` with one `_`, drop leading
/// dots, keep the last 120 **UTF-16 code units** (a cut low surrogate stays as the JS string did:
/// lone in JS, U+FFFD in Rust), and fall back to `file` when nothing remains.
pub fn safe_file_name(name: &str) -> String {
    let normalized: String = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .nfkc()
        .collect();
    let sanitized = UNSAFE.replace_all(&normalized, "_");
    let trimmed = LEADING_DOTS.replace(&sanitized, "");
    let units: Vec<u16> = trimmed.encode_utf16().collect();
    if units.is_empty() {
        return "file".into();
    }
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

/// A conservative extension allowlist; active formats such as SVG never execute.
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
