//! Error-code translation for the durable-facing adapters (port of `packages/env/src/errors.ts`).
//!
//! `pi-rs-durable` is ported in milestone 2.3b. This module defines the Rust equivalent of its `FileError` codes and
//! the mapping from a daemon `RemoteError`, so 2.3b only has to wrap these variants in its own error type.

use crate::connection::RemoteError;
use std::fmt;

/// The stable codes durable's `FileError` exposes to callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileErrorCode {
    Aborted,
    NotFound,
    PermissionDenied,
    NotDirectory,
    IsDirectory,
    Invalid,
    Unknown,
}

impl FileErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            FileErrorCode::Aborted => "aborted",
            FileErrorCode::NotFound => "not_found",
            FileErrorCode::PermissionDenied => "permission_denied",
            FileErrorCode::NotDirectory => "not_directory",
            FileErrorCode::IsDirectory => "is_directory",
            FileErrorCode::Invalid => "invalid",
            FileErrorCode::Unknown => "unknown",
        }
    }
}

impl fmt::Display for FileErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct FileError {
    pub code: FileErrorCode,
    pub message: String,
    pub path: Option<String>,
}

impl FileError {
    pub fn new(code: FileErrorCode, message: impl Into<String>, path: Option<&str>) -> FileError {
        FileError {
            code,
            message: message.into(),
            path: path.map(str::to_string),
        }
    }
}

impl fmt::Display for FileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for FileError {}

/// `NodeExecutionEnv`'s mapping of Node error codes to durable's file-error codes.
pub fn file_error_code(code: &str) -> FileErrorCode {
    match code {
        "aborted" => FileErrorCode::Aborted,
        "ENOENT" => FileErrorCode::NotFound,
        "EACCES" | "EPERM" => FileErrorCode::PermissionDenied,
        "ENOTDIR" => FileErrorCode::NotDirectory,
        "EISDIR" => FileErrorCode::IsDirectory,
        "EINVAL" | "SYMLINK" | "NOT_REGULAR" => FileErrorCode::Invalid,
        _ => FileErrorCode::Unknown,
    }
}

pub fn to_file_error(error: &RemoteError, fallback_path: Option<&str>) -> FileError {
    let path = error.path.as_deref().or(fallback_path);
    FileError::new(file_error_code(&error.code), error.message.clone(), path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_daemon_codes_to_the_durable_file_codes() {
        assert_eq!(file_error_code("ENOENT"), FileErrorCode::NotFound);
        assert_eq!(file_error_code("EACCES"), FileErrorCode::PermissionDenied);
        assert_eq!(file_error_code("EPERM"), FileErrorCode::PermissionDenied);
        assert_eq!(file_error_code("ENOTDIR"), FileErrorCode::NotDirectory);
        assert_eq!(file_error_code("EISDIR"), FileErrorCode::IsDirectory);
        assert_eq!(file_error_code("EINVAL"), FileErrorCode::Invalid);
        assert_eq!(file_error_code("SYMLINK"), FileErrorCode::Invalid);
        assert_eq!(file_error_code("NOT_REGULAR"), FileErrorCode::Invalid);
        assert_eq!(file_error_code("aborted"), FileErrorCode::Aborted);
        assert_eq!(file_error_code("timeout"), FileErrorCode::Unknown);
    }

    #[test]
    fn keeps_the_daemon_path_or_uses_the_fallback() {
        let error =
            RemoteError::new(&json!({ "code": "ENOENT", "message": "missing", "path": "/a" }));
        let file = to_file_error(&error, Some("/fallback"));
        assert_eq!(file.code, FileErrorCode::NotFound);
        assert_eq!(file.path.as_deref(), Some("/a"));
        let error = RemoteError::new(&json!({ "code": "EPERM", "message": "denied" }));
        assert_eq!(
            to_file_error(&error, Some("/fallback")).path.as_deref(),
            Some("/fallback")
        );
    }
}
