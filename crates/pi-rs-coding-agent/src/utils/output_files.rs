//! Output file creation, ported from `utils/output-files.ts`.
//!
//! Upstream uses `randomBytes(8).toString("hex")` for a 16-character lowercase hex suffix. The
//! Rust crate has no RNG dependency, so the suffix is the first 8 bytes of SHA-256 over the
//! monotonic clock, process id, and an atomic counter. The path is still unique per process and
//! `create_new` guarantees no existing file is ever reused.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

/// Output can carry private data: owner read/write only.
pub const OUTPUT_FILE_MODE: u32 = 0o600;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A new, unused path: `<tempdir>/<prefix>-<16 hex chars><extension>`. The extension includes its dot.
pub fn generate_output_path(prefix: &str, extension: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let digest = Sha256::digest(format!("{nanos}:{}:{sequence}", std::process::id()).as_bytes());
    let mut hex = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        hex.push_str(&format!("{byte:02x}"));
    }
    let file_name = format!("{prefix}-{hex}{extension}");
    std::env::temp_dir().join(file_name)
}

/// Open a new output file for streaming. `create_new` never follows a pre-placed symlink.
pub fn create_output_file_stream(prefix: &str, extension: &str) -> io::Result<(PathBuf, File)> {
    let mut last_error = None;
    for _ in 0..16 {
        let path = generate_output_path(prefix, extension);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(OUTPUT_FILE_MODE);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Could not allocate an output file",
        )
    }))
}

/// Write bytes to a new output file and return its path.
pub fn write_output_file(prefix: &str, extension: &str, data: &[u8]) -> io::Result<PathBuf> {
    let (path, mut file) = create_output_file_stream(prefix, extension)?;
    if let Err(error) = file.write_all(data) {
        let _ = std::fs::remove_file(&path);
        return Err(error);
    }
    Ok(path)
}

/// Where streamed output ended up: a complete file, or why it could not be saved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputFileResult {
    Saved { path: PathBuf },
    Failed { error: String },
}

/// An output file written while output streams in. Open and write failures (full disk, quota,
/// unwritable temp dir) never fail the caller: the first failure drops the file instead, removes
/// any partial file we created, and `close()` reports why.
pub struct OutputFileStream {
    file: Option<File>,
    result: OutputFileResult,
    opened: bool,
    closed: bool,
}

impl OutputFileStream {
    pub fn create(prefix: &str, extension: &str) -> Self {
        match create_output_file_stream(prefix, extension) {
            Ok((path, file)) => Self {
                file: Some(file),
                result: OutputFileResult::Saved { path },
                opened: true,
                closed: false,
            },
            Err(error) => Self {
                file: None,
                result: OutputFileResult::Failed {
                    error: error.to_string(),
                },
                opened: false,
                closed: true,
            },
        }
    }

    pub fn result(&self) -> &OutputFileResult {
        &self.result
    }

    pub fn write(&mut self, data: &[u8]) {
        if self.closed || !matches!(self.result, OutputFileResult::Saved { .. }) {
            return;
        }
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if let Err(error) = file.write_all(data) {
            self.fail(error.to_string());
        }
    }

    pub fn close(&mut self) -> OutputFileResult {
        if !self.closed {
            self.closed = true;
            if let Some(mut file) = self.file.take()
                && let Err(error) = file.flush().and_then(|()| file.sync_data().or(Ok(())))
            {
                self.fail(error.to_string());
            }
        }
        self.result.clone()
    }

    fn fail(&mut self, error: String) {
        let path = match &self.result {
            OutputFileResult::Saved { path } => Some(path.clone()),
            OutputFileResult::Failed { .. } => None,
        };
        self.file = None;
        if matches!(self.result, OutputFileResult::Saved { .. }) {
            self.result = OutputFileResult::Failed { error };
        }
        if self.opened
            && let Some(path) = path
        {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_path_shape_matches_upstream() {
        let path = generate_output_path("pi-bash", ".log");
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let rest = name.strip_prefix("pi-bash-").unwrap_or("");
        let (hex, ext) = rest.split_once('.').unwrap_or((rest, ""));
        assert_eq!(ext, "log");
        assert_eq!(hex.len(), 16);
        assert!(
            hex.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
    }

    #[test]
    fn generated_paths_are_distinct() {
        let left = generate_output_path("pi-test", ".txt");
        let right = generate_output_path("pi-test", ".txt");
        assert_ne!(left, right);
    }

    #[test]
    fn created_file_is_private_and_exclusive() -> Result<(), String> {
        let (path, _file) =
            create_output_file_stream("pi-test-exclusive", ".tmp").map_err(|e| e.to_string())?;
        assert!(path.exists());
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .is_err()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let mode = std::fs::metadata(&path).map_err(|e| e.to_string())?.mode() & 0o777;
            assert_eq!(mode, OUTPUT_FILE_MODE);
        }
        std::fs::remove_file(&path).map_err(|e| e.to_string())?;
        Ok(())
    }

    #[test]
    fn write_output_file_persists_bytes() -> Result<(), String> {
        let path = write_output_file("pi-test-write", ".bin", b"\x00\xffdata")
            .map_err(|e| e.to_string())?;
        let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
        assert_eq!(bytes, b"\x00\xffdata");
        std::fs::remove_file(&path).map_err(|e| e.to_string())?;
        Ok(())
    }

    #[test]
    fn output_file_stream_saves_and_ignores_writes_after_close() -> Result<(), String> {
        let mut stream = OutputFileStream::create("pi-test-stream", ".log");
        stream.write(b"hello");
        let result = stream.close();
        stream.write(b"late");
        let OutputFileResult::Saved { path } = result else {
            return Err(format!("expected Saved, got {result:?}"));
        };
        assert_eq!(
            std::fs::read_to_string(&path).map_err(|e| e.to_string())?,
            "hello"
        );
        std::fs::remove_file(&path).map_err(|e| e.to_string())?;
        Ok(())
    }
}
