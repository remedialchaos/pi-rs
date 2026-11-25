use std::{
    fs::{FileType, Metadata},
    io::{self, SeekFrom},
    time::UNIX_EPOCH,
};

use async_trait::async_trait;
use pi_rs_chord::context::Context;
use tokio::{
    fs::{self, File, OpenOptions, ReadDir},
    io::{AsyncReadExt, AsyncSeekExt},
};

use super::{
    BinaryReader, DirPage, DirReader, FileError, FileErrorCode, FileInfo, FileKind, LineScan,
    LineScanner, MAX_SAFE_INTEGER, ScanLinesOptions, StreamDecoder, TextLine, TextLineReader,
    check_abort,
};

const CHUNK_SIZE: usize = 64 * 1024;

pub(super) fn kind_of(file_type: FileType) -> Option<FileKind> {
    if file_type.is_file() {
        Some(FileKind::File)
    } else if file_type.is_dir() {
        Some(FileKind::Directory)
    } else if file_type.is_symlink() {
        Some(FileKind::Symlink)
    } else {
        None
    }
}

pub(super) fn info_of(path: &str, stats: &Metadata) -> Result<FileInfo, FileError> {
    let kind = kind_of(stats.file_type())
        .ok_or_else(|| FileError::at(FileErrorCode::Invalid, "Unsupported file type", path))?;
    let modified = stats
        .modified()
        .map_err(|error| FileError::io(error, Some(path)))?;
    let mtime_ms = match modified.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs_f64() * 1000.0,
        Err(error) => -error.duration().as_secs_f64() * 1000.0,
    };
    Ok(FileInfo {
        name: std::path::Path::new(path)
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
        path: path.to_owned(),
        kind,
        size: stats.len(),
        mtime_ms,
    })
}

fn closed(what: &str, path: &str) -> FileError {
    FileError::at(FileErrorCode::Invalid, format!("{what} is closed"), path)
}

async fn seek(file: &mut File, path: &str, position: u64) -> Result<(), FileError> {
    file.seek(SeekFrom::Start(position))
        .await
        .map_err(|error| FileError::io(error, Some(path)))?;
    Ok(())
}

pub(super) struct LocalTextLineReader {
    path: String,
    file: Option<File>,
    decoder: StreamDecoder,
    chunk: Box<[u8]>,
    byte_offset: u64,
    needs_seek: bool,
    buffered: String,
    start: usize,
    scanned: usize,
    ended: bool,
}

impl LocalTextLineReader {
    pub(super) fn new(file: File, path: String) -> Self {
        Self {
            path,
            file: Some(file),
            decoder: StreamDecoder::new(),
            chunk: vec![0; CHUNK_SIZE].into_boxed_slice(),
            byte_offset: 0,
            needs_seek: false,
            buffered: String::new(),
            start: 0,
            scanned: 0,
            ended: false,
        }
    }
}

#[async_trait]
impl TextLineReader for LocalTextLineReader {
    async fn read_line(&mut self, context: &Context) -> Result<Option<TextLine>, FileError> {
        check_abort(context, Some(&self.path))?;
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| closed("Text line reader", &self.path))?;
        loop {
            if let Some(newline) = self.buffered.as_bytes()[self.scanned..]
                .iter()
                .position(|byte| *byte == b'\n')
            {
                let end = self.scanned + newline;
                let text = self.buffered[self.start..end].to_owned();
                self.start = end + 1;
                self.scanned = self.start;
                return Ok(Some(TextLine {
                    text,
                    terminated: true,
                }));
            }
            if self.ended {
                if self.start == self.buffered.len() {
                    return Ok(None);
                }
                let mut text = std::mem::take(&mut self.buffered);
                text.drain(..self.start);
                self.start = 0;
                self.scanned = 0;
                return Ok(Some(TextLine {
                    text,
                    terminated: false,
                }));
            }
            if self.start != 0 {
                self.buffered.drain(..self.start);
                self.start = 0;
            }
            self.scanned = self.buffered.len();
            if self.needs_seek {
                seek(file, &self.path, self.byte_offset).await?;
            }
            self.needs_seek = true;
            let count = file
                .read(&mut self.chunk)
                .await
                .map_err(|error| FileError::io(error, Some(&self.path)))?;
            check_abort(context, Some(&self.path))?;
            self.needs_seek = false;
            self.byte_offset += count as u64;
            self.ended = count == 0;
            self.decoder.decode_into(
                (!self.ended).then_some(&self.chunk[..count]),
                &mut self.buffered,
            );
        }
    }

    async fn close(&mut self, _context: &Context) {
        self.file = None;
        self.buffered = String::new();
        self.chunk = Box::default();
    }
}

pub(super) struct LocalBinaryReader {
    file: Option<File>,
    path: String,
}

impl LocalBinaryReader {
    pub(super) async fn open(
        path: String,
        no_follow: bool,
        context: &Context,
    ) -> Result<Self, FileError> {
        check_abort(context, Some(&path))?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        options.custom_flags(linux_open_flags(no_follow));
        #[cfg(any(
            target_os = "macos",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "dragonfly"
        ))]
        options.custom_flags(0x4 | if no_follow { 0x100 } else { 0 }); // O_NONBLOCK | O_NOFOLLOW (Darwin/BSD)
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "dragonfly"
        )))]
        {
            if no_follow && is_link(&path).await {
                return Err(FileError::at(
                    FileErrorCode::Invalid,
                    "Refusing to follow a symbolic link",
                    &path,
                ));
            }
            let stats = fs::metadata(&path)
                .await
                .map_err(|error| FileError::io(error, Some(&path)))?;
            require_file(&path, &stats)?;
        }
        let file = match options.open(&path).await {
            Ok(file) => file,
            Err(error) => {
                let mut error = FileError::io(error, Some(&path));
                if no_follow && (symlink_errno(error.cause.as_ref()) || is_link(&path).await) {
                    error.code = FileErrorCode::Invalid;
                    error.message = "Refusing to follow a symbolic link".to_owned();
                }
                return Err(error);
            }
        };
        let stats = file
            .metadata()
            .await
            .map_err(|error| FileError::io(error, Some(&path)))?;
        require_file(&path, &stats)?;
        check_abort(context, Some(&path))?;
        Ok(Self {
            file: Some(file),
            path,
        })
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn linux_open_flags(no_follow: bool) -> i32 {
    // O_* values are assigned per Linux architecture, not per libc.
    let nonblock = if cfg!(any(target_arch = "mips", target_arch = "mips64")) {
        0x80
    } else if cfg!(any(target_arch = "sparc", target_arch = "sparc64")) {
        0x4000
    } else {
        0x800
    };
    let nofollow = if cfg!(all(target_os = "android", target_arch = "riscv64")) {
        0x400000
    } else if cfg!(any(
        target_arch = "arm",
        target_arch = "aarch64",
        target_arch = "powerpc",
        target_arch = "powerpc64",
        target_arch = "m68k"
    )) {
        0x8000
    } else {
        0x20000
    };
    nonblock | if no_follow { nofollow } else { 0 }
}

async fn is_link(path: &str) -> bool {
    fs::symlink_metadata(path)
        .await
        .is_ok_and(|stats| stats.is_symlink())
}

fn symlink_errno(error: Option<&io::Error>) -> bool {
    let errno = error.and_then(io::Error::raw_os_error);
    if cfg!(any(target_os = "linux", target_os = "android")) {
        errno == Some(40) // ELOOP (Linux x86/ARM)
    } else if cfg!(unix) {
        matches!(errno, Some(31 | 62)) // EMLINK or ELOOP (Darwin/BSD)
    } else {
        false
    }
}

fn require_file(path: &str, stats: &Metadata) -> Result<(), FileError> {
    if stats.is_file() {
        Ok(())
    } else if stats.is_dir() {
        Err(FileError::at(
            FileErrorCode::IsDirectory,
            "Is a directory",
            path,
        ))
    } else {
        Err(FileError::at(
            FileErrorCode::Invalid,
            "Not a regular file",
            path,
        ))
    }
}

#[async_trait]
impl BinaryReader for LocalBinaryReader {
    async fn info(&self, context: &Context) -> Result<FileInfo, FileError> {
        check_abort(context, Some(&self.path))?;
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| closed("Binary reader", &self.path))?;
        let stats = file
            .metadata()
            .await
            .map_err(|error| FileError::io(error, Some(&self.path)))?;
        info_of(&self.path, &stats)
    }

    async fn read(
        &mut self,
        offset: u64,
        length: u64,
        context: &Context,
    ) -> Result<Vec<u8>, FileError> {
        check_abort(context, Some(&self.path))?;
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| closed("Binary reader", &self.path))?;
        if offset > MAX_SAFE_INTEGER || length > MAX_SAFE_INTEGER {
            return Err(FileError::at(
                FileErrorCode::Invalid,
                "Offset and length must be non-negative safe integers",
                &self.path,
            ));
        }
        let mut bytes = Vec::new();
        if length != 0 {
            seek(file, &self.path, offset).await?;
            context
                .await_with(file.take(length).read_to_end(&mut bytes))
                .await
                .map_err(|_| FileError::at(FileErrorCode::Aborted, "aborted", &self.path))?
                .map_err(|error| FileError::io(error, Some(&self.path)))?;
        }
        Ok(bytes)
    }

    async fn scan_lines(
        &mut self,
        options: ScanLinesOptions,
        context: &Context,
    ) -> Result<LineScan, FileError> {
        check_abort(context, Some(&self.path))?;
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| closed("Binary reader", &self.path))?;
        let mut scanner =
            LineScanner::new(options.start_line, options.end_line).map_err(|mut error| {
                error.path = Some(self.path.clone());
                error
            })?;
        seek(file, &self.path, 0).await?;
        let mut chunk = vec![0; CHUNK_SIZE];
        loop {
            let count = file
                .read(&mut chunk)
                .await
                .map_err(|error| FileError::io(error, Some(&self.path)))?;
            check_abort(context, Some(&self.path))?;
            if count == 0 {
                return Ok(scanner.finish());
            }
            scanner.push(&chunk[..count]);
        }
    }

    async fn close(&mut self, _context: &Context) {
        self.file = None;
    }
}

pub(super) struct LocalDirReader {
    dir: Option<ReadDir>,
    path: String,
    done: bool,
}

impl LocalDirReader {
    pub(super) fn new(dir: ReadDir, path: String) -> Self {
        Self {
            dir: Some(dir),
            path,
            done: false,
        }
    }
}

#[async_trait]
impl DirReader for LocalDirReader {
    async fn next(&mut self, max_entries: u64, context: &Context) -> Result<DirPage, FileError> {
        check_abort(context, Some(&self.path))?;
        let dir = self
            .dir
            .as_mut()
            .ok_or_else(|| closed("Directory reader", &self.path))?;
        if max_entries == 0 || max_entries > MAX_SAFE_INTEGER {
            return Err(FileError::at(
                FileErrorCode::Invalid,
                "maxEntries must be a positive safe integer",
                &self.path,
            ));
        }
        let mut entries = Vec::new();
        while !self.done && (entries.len() as u64) < max_entries {
            let Some(entry) = dir
                .next_entry()
                .await
                .map_err(|error| FileError::io(error, Some(&self.path)))?
            else {
                self.done = true;
                break;
            };
            check_abort(context, Some(&self.path))?;
            let path = entry.path();
            let stats = match fs::symlink_metadata(&path).await {
                Ok(stats) => stats,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(FileError::io(error, Some(&path.to_string_lossy()))),
            };
            if let Ok(info) = info_of(&path.to_string_lossy(), &stats) {
                entries.push(info);
            }
        }
        Ok(DirPage {
            entries,
            done: self.done,
        })
    }

    async fn close(&mut self, _context: &Context) {
        self.dir = None;
    }
}
