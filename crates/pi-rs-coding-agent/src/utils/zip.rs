//! ZIP archive helpers, ported from `utils/zip.ts`.
//!
//! Upstream writes local headers, a central directory and an EOCD by hand with deflate and CRC32.
//! The `zip` crate produces the same interoperable ZIP layout; its metadata fields are not exposed
//! through this API. Entry names are validated on extraction to prevent `../` path traversal.

use std::fs::File;
use std::io::{Cursor, Seek, Write};
use std::path::{Component, Path, PathBuf};

use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZipEntry {
    pub name: String,
    pub data: Vec<u8>,
}

fn write_entries<W: Write + Seek>(
    writer: &mut ZipWriter<W>,
    entries: &[ZipEntry],
) -> Result<(), String> {
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);
    for entry in entries {
        writer
            .start_file(&entry.name, options)
            .map_err(|error| format!("Failed to add ZIP entry {}: {error}", entry.name))?;
        writer
            .write_all(&entry.data)
            .map_err(|error| format!("Failed to write ZIP entry {}: {error}", entry.name))?;
    }
    Ok(())
}

/// Build an archive in memory. Useful for callers that need bytes before writing a file.
pub fn create_zip_bytes(entries: &[ZipEntry]) -> Result<Vec<u8>, String> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    write_entries(&mut writer, entries)?;
    let cursor = writer.finish().map_err(|error| error.to_string())?;
    Ok(cursor.into_inner())
}

pub fn write_zip_archive(path: &Path, entries: &[ZipEntry]) -> Result<(), String> {
    let file = File::create(path).map_err(|error| error.to_string())?;
    let mut writer = ZipWriter::new(file);
    write_entries(&mut writer, entries)?;
    writer.finish().map_err(|error| error.to_string())?;
    Ok(())
}

/// Reject absolute paths, drive prefixes, and parent traversal in archive member names.
fn safe_relative_name(name: &str) -> Option<PathBuf> {
    let normalized = name.replace('\\', "/");
    let bytes = normalized.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return None;
    }
    let path = Path::new(&normalized);
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => result.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if result.as_os_str().is_empty() {
        None
    } else {
        Some(result)
    }
}

/// Extract all entries under `dest`, returning the paths written. Unsafe member names are rejected
/// before any file is created.
pub fn extract_zip_bytes(bytes: &[u8], dest: &Path) -> Result<Vec<PathBuf>, String> {
    let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(|error| error.to_string())?;
    let mut written = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| format!("Failed to read ZIP entry {index}: {error}"))?;
        let relative = safe_relative_name(entry.name())
            .ok_or_else(|| format!("Unsafe ZIP entry name: {}", entry.name()))?;
        let target = dest.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let mut file = File::create(&target).map_err(|error| error.to_string())?;
        std::io::copy(&mut entry, &mut file)
            .map_err(|error| format!("Failed to extract {}: {error}", entry.name()))?;
        written.push(target);
    }
    Ok(written)
}

pub fn extract_zip_archive(path: &Path, dest: &Path) -> Result<Vec<PathBuf>, String> {
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    extract_zip_bytes(&bytes, dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_archive_entries() -> Result<(), String> {
        let entries = vec![
            ZipEntry {
                name: "a.txt".to_string(),
                data: b"hello".to_vec(),
            },
            ZipEntry {
                name: "dir/b.bin".to_string(),
                data: vec![0, 255, 10],
            },
        ];
        let bytes = create_zip_bytes(&entries)?;
        assert_eq!(&bytes[0..2], b"PK");
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let paths = extract_zip_bytes(&bytes, dir.path())?;
        assert_eq!(paths.len(), 2);
        assert_eq!(
            std::fs::read(&paths[0]).map_err(|e| e.to_string())?,
            b"hello"
        );
        assert_eq!(
            std::fs::read(&paths[1]).map_err(|e| e.to_string())?,
            vec![0, 255, 10]
        );
        Ok(())
    }

    #[test]
    fn traversal_entry_is_rejected() -> Result<(), String> {
        let bytes = create_zip_bytes(&[ZipEntry {
            name: "../escape.txt".to_string(),
            data: b"bad".to_vec(),
        }])?;
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        assert!(extract_zip_bytes(&bytes, dir.path()).is_err());
        assert!(
            !dir.path()
                .parent()
                .unwrap_or(dir.path())
                .join("escape.txt")
                .exists()
        );
        Ok(())
    }

    #[test]
    fn absolute_entry_is_rejected() {
        assert_eq!(safe_relative_name("/etc/passwd"), None);
        assert_eq!(safe_relative_name("C:\\Windows\\x"), None);
        assert_eq!(
            safe_relative_name("good/./file.txt").as_deref(),
            Some(Path::new("good/file.txt"))
        );
    }

    #[test]
    fn archive_file_roundtrip() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let archive = dir.path().join("out.zip");
        write_zip_archive(
            &archive,
            &[ZipEntry {
                name: "x.txt".to_string(),
                data: b"x".to_vec(),
            }],
        )?;
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
        assert_eq!(extract_zip_archive(&archive, &out)?.len(), 1);
        Ok(())
    }
}
