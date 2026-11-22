use super::{private_directory, private_store, safe_relative};
use crate::{
    Error, Result,
    sqlite::{Options, SqliteDatabase},
    store::StoreEngine,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io::Read, path::Path};
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TarEntry {
    pub path: String,
    pub bytes: Vec<u8>,
    pub directory: bool,
}
fn field(block: &mut [u8], offset: usize, length: usize, value: &str) -> Result<()> {
    if value.len() > length {
        return Err(Error::Invalid("tar field overflow".into()));
    }
    block[offset..offset + value.len()].copy_from_slice(value.as_bytes());
    Ok(())
}
pub fn write_tar(entries: &[TarEntry]) -> Result<Vec<u8>> {
    let mut tar = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for entry in entries {
        safe_relative(&entry.path)?;
        if !seen.insert(&entry.path) {
            return Err(Error::Invalid("duplicate tar path".into()));
        }
        let mut header = [0; 512];
        let (prefix, name) = if entry.path.len() > 100 {
            let (prefix, name) = entry
                .path
                .rsplit_once('/')
                .ok_or_else(|| Error::Invalid("tar path too long".into()))?;
            if prefix.len() > 155 || name.len() > 100 {
                return Err(Error::Invalid("tar path too long".into()));
            }
            (prefix, name)
        } else {
            ("", entry.path.as_str())
        };
        field(&mut header, 0, 100, name)?;
        field(&mut header, 100, 8, "0000644")?;
        field(&mut header, 108, 8, "0000000")?;
        field(&mut header, 116, 8, "0000000")?;
        field(&mut header, 124, 12, &format!("{:011o}", entry.bytes.len()))?;
        field(&mut header, 136, 12, "00000000000")?;
        header[148..156].fill(b' ');
        header[156] = if entry.directory { b'5' } else { b'0' };
        field(&mut header, 257, 8, "ustar\x0000")?;
        field(&mut header, 345, 155, prefix)?;
        let sum = header.iter().map(|b| u64::from(*b)).sum::<u64>();
        field(&mut header, 148, 8, &format!("{sum:06o}\0 "))?;
        tar.extend_from_slice(&header);
        tar.extend_from_slice(&entry.bytes);
        tar.resize(tar.len().next_multiple_of(512), 0);
    }
    tar.resize(tar.len() + 1024, 0);
    Ok(tar)
}
fn text(field: &[u8]) -> Result<&str> {
    std::str::from_utf8(field.split(|b| *b == 0).next().unwrap_or_default())
        .map_err(|_| Error::Invalid("non UTF-8 tar field".into()))
}
fn octal(field: &[u8]) -> Result<u64> {
    let value = text(field)?.trim();
    if value.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(value, 8).map_err(|_| Error::Invalid("invalid tar number".into()))
}
#[derive(Clone, Copy)]
pub struct ArchiveLimits {
    pub bytes: u64,
    pub entries: usize,
}
impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            bytes: 4 * 1024 * 1024 * 1024,
            entries: 100_000,
        }
    }
}
pub fn read_tar(bytes: &[u8], limits: ArchiveLimits) -> Result<Vec<TarEntry>> {
    let mut entries = Vec::new();
    let mut at = 0;
    let mut total = 0u64;
    let mut seen = std::collections::BTreeSet::new();
    loop {
        let header = bytes
            .get(at..at + 512)
            .ok_or_else(|| Error::Invalid("truncated tar header".into()))?;
        at += 512;
        if header.iter().all(|b| *b == 0) {
            if bytes
                .get(at..at + 512)
                .is_none_or(|b| b.iter().any(|b| *b != 0))
                || bytes[at + 512..].iter().any(|b| *b != 0)
            {
                return Err(Error::Invalid("invalid tar end".into()));
            }
            return Ok(entries);
        }
        let expected = octal(&header[148..156])?;
        let actual = header
            .iter()
            .enumerate()
            .map(|(i, b)| {
                if (148..156).contains(&i) {
                    32
                } else {
                    u64::from(*b)
                }
            })
            .sum::<u64>();
        if expected != actual {
            return Err(Error::Invalid("tar checksum mismatch".into()));
        }
        if &header[257..263] != b"ustar\0" {
            return Err(Error::Invalid("not a ustar archive".into()));
        }
        let name = text(&header[..100])?;
        let prefix = text(&header[345..500])?;
        let path = if prefix.is_empty() {
            name.into()
        } else {
            format!("{prefix}/{name}")
        };
        safe_relative(&path)?;
        if path.len() > 255 || !seen.insert(path.clone()) {
            return Err(Error::Invalid("invalid or duplicate tar path".into()));
        }
        let size = octal(&header[124..136])?;
        total = total
            .checked_add(size)
            .ok_or_else(|| Error::Invalid("tar size overflow".into()))?;
        if total > limits.bytes || entries.len() >= limits.entries {
            return Err(Error::Invalid("archive limit exceeded".into()));
        }
        let directory = match header[156] {
            0 | b'0' => false,
            b'5' if size == 0 => true,
            _ => return Err(Error::Invalid("tar special file refused".into())),
        };
        let length = usize::try_from(size)
            .map_err(|_| Error::Invalid("tar entry exceeds address space".into()))?;
        let end = at
            .checked_add(length)
            .ok_or_else(|| Error::Invalid("tar size overflow".into()))?;
        let data = bytes
            .get(at..end)
            .ok_or_else(|| Error::Invalid("truncated tar body".into()))?;
        entries.push(TarEntry {
            path,
            bytes: data.to_vec(),
            directory,
        });
        at = end
            .checked_add((512 - length % 512) % 512)
            .ok_or_else(|| Error::Invalid("tar offset overflow".into()))?;
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ArchiveManifest {
    pub format: u32,
    pub app: String,
    pub api: String,
    pub address: ArchiveAddress,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArchiveAddress {
    pub kind: String,
    pub key: String,
}
pub async fn pack_data(
    manifest: ArchiveManifest,
    store: &StoreEngine,
    files: &Path,
    server_archive: Option<Vec<u8>>,
) -> Result<Vec<u8>> {
    if manifest.format != 2 {
        return Err(Error::Invalid("unsupported archive format".into()));
    }
    let gzip = store.export_gzip().await?;
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(&gzip[..]).read_to_end(&mut raw)?;
    let mut entries = vec![
        TarEntry {
            path: "manifest.json".into(),
            bytes: serde_json::to_vec(&manifest)?,
            directory: false,
        },
        TarEntry {
            path: "store.archive".into(),
            bytes: raw,
            directory: false,
        },
    ];
    if let Some(bytes) = server_archive {
        entries.push(TarEntry {
            path: "server.archive".into(),
            bytes,
            directory: false,
        });
    }
    fn collect(root: &Path, path: &Path, entries: &mut Vec<TarEntry>) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        let mut children = std::fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
        children.sort_by_key(|e| e.file_name());
        for child in children {
            let kind = child.file_type()?;
            if child.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if kind.is_symlink() {
                return Err(Error::Invalid("archive files contain a symlink".into()));
            }
            if kind.is_dir() {
                collect(root, &child.path(), entries)?;
            } else if kind.is_file() {
                let relative = child
                    .path()
                    .strip_prefix(root)
                    .map_err(|_| Error::Invalid("archive file escapes root".into()))?
                    .to_string_lossy()
                    .replace('\\', "/");
                entries.push(TarEntry {
                    path: format!("files/{relative}"),
                    bytes: std::fs::read(child.path())?,
                    directory: false,
                });
            }
        }
        Ok(())
    }
    collect(files, files, &mut entries)?;
    write_tar(&entries)
}
/// Validate the whole archive before touching staging. The caller chooses when to swap it into service.
pub async fn unpack_staging(
    bytes: &[u8],
    app: &str,
    address: &ArchiveAddress,
    staging: &Path,
    limits: ArchiveLimits,
) -> Result<(ArchiveManifest, StoreEngine)> {
    let entries = read_tar(bytes, limits)?;
    if entries
        .first()
        .is_none_or(|e| e.path != "manifest.json" || e.directory || e.bytes.len() > 64 * 1024)
    {
        return Err(Error::Invalid(
            "archive must start with a bounded manifest.json".into(),
        ));
    }
    let mut by_path = entries
        .into_iter()
        .map(|e| (e.path.clone(), e))
        .collect::<BTreeMap<_, _>>();
    let manifest: ArchiveManifest = serde_json::from_slice(
        &by_path
            .remove("manifest.json")
            .ok_or_else(|| Error::Invalid("archive has no manifest".into()))?
            .bytes,
    )?;
    if manifest.format != 2 || manifest.app != app || manifest.address != *address {
        return Err(Error::Invalid(
            "archive manifest does not match target".into(),
        ));
    }
    let store = by_path
        .remove("store.archive")
        .ok_or_else(|| Error::Invalid("archive has no store".into()))?;
    for entry in by_path.values() {
        if entry.path != "server.archive" && !entry.path.starts_with("files/") {
            return Err(Error::Invalid("unexpected archive entry".into()));
        }
    }
    if staging.exists() {
        return Err(Error::Invalid("archive staging already exists".into()));
    }
    private_directory(staging)?;
    let result = async {
        let path = staging.join("store.sqlite");
        private_store(&path)?;
        let db = SqliteDatabase::open(&path, Options::default()).await?;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &store.bytes)?;
        let gzip = encoder.finish()?;
        let imported = StoreEngine::import_gzip(db.clone(), gzip, true).await;
        let imported = match imported {
            Ok(store) => store,
            Err(error) => {
                let _ = db.close().await;
                return Err(error);
            }
        };
        for entry in by_path.values() {
            if let Some(path) = entry.path.strip_prefix("files/") {
                let target = staging.join("files").join(path);
                if entry.directory {
                    private_directory(&target)?;
                } else {
                    if let Some(parent) = target.parent() {
                        private_directory(parent)?;
                    }
                    let mut options = std::fs::OpenOptions::new();
                    options.create_new(true).write(true);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::OpenOptionsExt;
                        options.mode(0o600);
                    }
                    use std::io::Write;
                    options.open(target)?.write_all(&entry.bytes)?;
                }
            } else if entry.path == "server.archive" {
                private_store(&staging.join("server.archive"))?;
                std::fs::write(staging.join("server.archive"), &entry.bytes)?;
            }
        }
        Ok((manifest, imported))
    }
    .await;
    if result.is_err() {
        let _ = std::fs::remove_dir_all(staging);
    }
    result
}
