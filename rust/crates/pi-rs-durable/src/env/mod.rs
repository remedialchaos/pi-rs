//! Portable file, watch and shell capabilities and their local OS adapter.

mod decode;
mod line_scan;
mod local;
mod readers;
mod shell;
mod watch;

pub use decode::{RangeDecoder, StreamDecoder, range_decoder, starts_with_bom};
pub use line_scan::LineScanner;
pub use local::{LocalEnvOptions, LocalExecutionEnv};

use std::{collections::BTreeMap, io};

use async_trait::async_trait;
use pi_rs_chord::context::Context;
use serde::{Deserialize, Serialize};

const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    File,
    Directory,
    Symlink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileErrorCode {
    Aborted,
    NotFound,
    PermissionDenied,
    NotDirectory,
    IsDirectory,
    Invalid,
    NotSupported,
    Unknown,
}

#[derive(Debug, thiserror::Error, Serialize, Deserialize)]
#[error("{message}")]
pub struct FileError {
    pub code: FileErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[source]
    #[serde(skip)]
    pub cause: Option<io::Error>,
}

impl FileError {
    pub fn new(code: FileErrorCode, message: impl Into<String>, path: Option<String>) -> Self {
        Self {
            code,
            message: message.into(),
            path,
            cause: None,
        }
    }

    fn at(code: FileErrorCode, message: impl Into<String>, path: &str) -> Self {
        Self::new(code, message, Some(path.to_owned()))
    }

    fn io(error: io::Error, path: Option<&str>) -> Self {
        let code = match error.kind() {
            io::ErrorKind::NotFound => FileErrorCode::NotFound,
            io::ErrorKind::PermissionDenied => FileErrorCode::PermissionDenied,
            io::ErrorKind::NotADirectory => FileErrorCode::NotDirectory,
            io::ErrorKind::IsADirectory => FileErrorCode::IsDirectory,
            io::ErrorKind::InvalidInput => FileErrorCode::Invalid,
            io::ErrorKind::Unsupported => FileErrorCode::NotSupported,
            _ => FileErrorCode::Unknown,
        };
        Self {
            code,
            message: error.to_string(),
            path: path.map(str::to_owned),
            cause: Some(error),
        }
    }
}

fn check_abort(context: &Context, path: Option<&str>) -> Result<(), FileError> {
    if context.is_aborted() {
        Err(FileError::new(
            FileErrorCode::Aborted,
            "aborted",
            path.map(str::to_owned),
        ))
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionErrorCode {
    Aborted,
    Timeout,
    ShellUnavailable,
    SpawnError,
    CallbackError,
    Unknown,
}

#[derive(Debug, thiserror::Error, Serialize, Deserialize)]
#[error("{message}")]
#[serde(rename_all = "camelCase")]
pub struct ExecutionError {
    pub code: ExecutionErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spill_path: Option<String>,
    #[source]
    #[serde(skip)]
    pub cause: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl ExecutionError {
    pub fn new(code: ExecutionErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            spill_path: None,
            cause: None,
        }
    }

    fn caused_by(mut self, cause: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.cause = Some(Box::new(cause));
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileInfo {
    pub name: String,
    pub path: String,
    pub kind: FileKind,
    pub size: u64,
    pub mtime_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextLine {
    pub text: String,
    pub terminated: bool,
}

#[async_trait]
pub trait TextLineReader: Send {
    async fn read_line(&mut self, context: &Context) -> Result<Option<TextLine>, FileError>;
    async fn close(&mut self, context: &Context);
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenBinaryOptions {
    #[serde(default)]
    pub no_follow: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanLinesOptions {
    pub start_line: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LineScan {
    pub newlines: u64,
    pub start: u64,
    pub end: u64,
    pub first_line_end: u64,
    pub last_line_start: u64,
    pub selected_bytes: u64,
    pub first_line_bytes: u64,
}

/// A regular file held open, even when its original path is replaced.
#[async_trait]
pub trait BinaryReader: Send {
    async fn info(&self, context: &Context) -> Result<FileInfo, FileError>;
    async fn read(
        &mut self,
        offset: u64,
        length: u64,
        context: &Context,
    ) -> Result<Vec<u8>, FileError>;
    async fn scan_lines(
        &mut self,
        options: ScanLinesOptions,
        context: &Context,
    ) -> Result<LineScan, FileError>;
    async fn close(&mut self, context: &Context);
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DirPage {
    pub entries: Vec<FileInfo>,
    pub done: bool,
}

#[async_trait]
pub trait DirReader: Send {
    /// Up to `max_entries` in filesystem order. After a failed or aborted call, close the reader.
    async fn next(&mut self, max_entries: u64, context: &Context) -> Result<DirPage, FileError>;
    async fn close(&mut self, context: &Context);
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFilesOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gitignore: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_entries: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileListEntry {
    /// Root-relative with `/` separators. Symbolic links are listed but not followed.
    pub path: String,
    pub kind: FileKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileListing {
    /// Depth first, parents before children, siblings in byte order.
    pub entries: Vec<FileListEntry>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateDirOptions {
    #[serde(default = "default_recursive")]
    pub recursive: bool,
}

const fn default_recursive() -> bool {
    true
}

impl Default for CreateDirOptions {
    fn default() -> Self {
        Self { recursive: true }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveOptions {
    #[serde(default)]
    pub recursive: bool,
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TempFileOptions<'a> {
    #[serde(borrow, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<&'a str>,
    #[serde(borrow, skip_serializing_if = "Option::is_none")]
    pub suffix: Option<&'a str>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchExclude {
    #[serde(default)]
    pub hidden: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchTarget {
    pub path: String,
    #[serde(default)]
    pub recursive: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude: Option<WatchExclude>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum FileChange {
    /// A reported path covers its subtree. Spurious notifications are allowed.
    Paths { paths: Vec<String> },
    /// Coverage was uncertain: rescan the watched paths.
    Overflow,
    /// Watching stopped; there will be no further callbacks.
    Failed { error: FileError },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WatchMode {
    Native,
    Polling,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<WatchMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_interval_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_directories: Option<usize>,
}

#[async_trait]
pub trait FileWatcher: Send {
    fn mode(&self) -> WatchMode;
    /// Idempotent. No new callback starts after this returns.
    async fn stop(&mut self, context: &Context);
}

pub type WatchCallback = Box<dyn FnMut(FileChange) + Send + 'static>;

#[async_trait]
pub trait FileSystem: Send + Sync {
    fn id(&self) -> &str;
    fn cwd(&self) -> &str;
    async fn absolute_path(&self, path: &str, context: &Context) -> Result<String, FileError>;
    async fn join_path(&self, parts: &[&str], context: &Context) -> Result<String, FileError>;
    async fn read_text_file(&self, path: &str, context: &Context) -> Result<String, FileError>;
    async fn open_text_line_reader(
        &self,
        path: &str,
        context: &Context,
    ) -> Result<Box<dyn TextLineReader>, FileError>;
    async fn read_text_lines(
        &self,
        path: &str,
        max_lines: Option<usize>,
        context: &Context,
    ) -> Result<Vec<String>, FileError> {
        if max_lines == Some(0) {
            return Ok(Vec::new());
        }
        let mut reader = self.open_text_line_reader(path, context).await?;
        let result = async {
            let mut lines = Vec::new();
            while max_lines.is_none_or(|max| lines.len() < max) {
                let Some(line) = reader.read_line(context).await? else {
                    break;
                };
                lines.push(line.text);
            }
            Ok(lines)
        }
        .await;
        reader.close(context).await;
        result
    }
    async fn read_binary_file(&self, path: &str, context: &Context) -> Result<Vec<u8>, FileError>;
    async fn open_binary_reader(
        &self,
        path: &str,
        options: Option<OpenBinaryOptions>,
        context: &Context,
    ) -> Result<Box<dyn BinaryReader>, FileError>;
    async fn write_file(
        &self,
        path: &str,
        content: &[u8],
        context: &Context,
    ) -> Result<(), FileError>;
    async fn append_file(
        &self,
        path: &str,
        content: &[u8],
        context: &Context,
    ) -> Result<(), FileError>;
    async fn truncate_file(
        &self,
        path: &str,
        size: u64,
        context: &Context,
    ) -> Result<(), FileError>;
    async fn flush_file(&self, path: &str, context: &Context) -> Result<(), FileError>;
    async fn rename_file(
        &self,
        source: &str,
        destination: &str,
        context: &Context,
    ) -> Result<(), FileError>;
    async fn file_info(&self, path: &str, context: &Context) -> Result<FileInfo, FileError>;
    async fn list_dir(&self, path: &str, context: &Context) -> Result<Vec<FileInfo>, FileError>;
    async fn list_files(
        &self,
        path: &str,
        _options: Option<ListFilesOptions>,
        _context: &Context,
    ) -> Result<FileListing, FileError> {
        Err(FileError::at(
            FileErrorCode::NotSupported,
            "Recursive listing is not supported",
            path,
        ))
    }
    async fn open_dir_reader(
        &self,
        path: &str,
        context: &Context,
    ) -> Result<Box<dyn DirReader>, FileError>;
    async fn watch(
        &self,
        targets: &[WatchTarget],
        on_change: WatchCallback,
        context: &Context,
    ) -> Result<Box<dyn FileWatcher>, FileError>;
    async fn canonical_path(&self, path: &str, context: &Context) -> Result<String, FileError>;
    async fn exists(&self, path: &str, context: &Context) -> Result<bool, FileError> {
        match self.file_info(path, context).await {
            Ok(_) => Ok(true),
            Err(error) if error.code == FileErrorCode::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
    async fn create_dir(
        &self,
        path: &str,
        options: Option<CreateDirOptions>,
        context: &Context,
    ) -> Result<(), FileError>;
    async fn remove(
        &self,
        path: &str,
        options: Option<RemoveOptions>,
        context: &Context,
    ) -> Result<(), FileError>;
    async fn create_temp_dir(
        &self,
        prefix: Option<&str>,
        context: &Context,
    ) -> Result<String, FileError>;
    async fn create_temp_file(
        &self,
        options: Option<TempFileOptions<'_>>,
        context: &Context,
    ) -> Result<String, FileError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellSpillOptions {
    pub after_bytes: usize,
    pub after_lines: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellExecResult {
    pub exit_code: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spill_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellOutputWindow {
    pub max_bytes: usize,
    pub max_lines: usize,
    pub min_interval_ms: u64,
    pub bytes_per_second: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellOutputSkip {
    pub bytes: usize,
    pub newlines: usize,
    pub ends_with_newline: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellOutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellOutputInfo {
    pub stream: ShellOutputStream,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<ShellOutputSkip>,
}

pub type ShellOutputCallback<'a> =
    dyn FnMut(&str, &Context, ShellOutputInfo) -> Result<(), ExecutionError> + Send + 'a;

#[derive(Default)]
pub struct ShellExecOptions<'a> {
    pub cwd: Option<&'a str>,
    pub env: Option<&'a BTreeMap<String, String>>,
    pub inherit_env: Option<bool>,
    /// Timeout in seconds, including fractional seconds.
    pub timeout: Option<f64>,
    pub on_output: Option<&'a mut ShellOutputCallback<'a>>,
    pub spill: Option<ShellSpillOptions>,
    /// Optional hint for environments that transfer output over a slow link. Local exec delivers all output.
    pub window: Option<ShellOutputWindow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellCommand<'a> {
    Script(&'a str),
    Argv(&'a [&'a str]),
}

#[async_trait]
pub trait Shell: Send + Sync {
    /// An argv command bypasses shell parsing. Aborting kills only this command's processes.
    async fn exec(
        &self,
        command: ShellCommand<'_>,
        options: Option<ShellExecOptions<'_>>,
        context: &Context,
    ) -> Result<ShellExecResult, ExecutionError>;
    /// Kill all commands owned by this environment, not by other instances.
    async fn cleanup(&self, context: &Context);
}

pub trait ExecutionEnv: FileSystem + Shell {}

impl<T: FileSystem + Shell + ?Sized> ExecutionEnv for T {}
