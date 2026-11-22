//! Native application kit, private persistence, authentication and per-address archives.
use crate::{Error, Result};
use base64::Engine;
use std::{
    io::Write,
    path::{Path, PathBuf},
};
mod archive;
mod auth;
mod files;
mod runtime;
pub use archive::*;
pub use auth::*;
pub use files::*;
pub use runtime::*;
pub fn new_token(bytes: usize) -> String {
    let bytes = (0..bytes).map(|_| rand::random::<u8>()).collect::<Vec<_>>();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}
pub fn safe_relative(path: &str) -> Result<()> {
    if path.is_empty()
        || path.contains(['\\', '\0', ':'])
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(Error::Invalid("unsafe relative path".into()));
    }
    Ok(())
}
pub fn private_directory(path: &Path) -> Result<()> {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(Error::Invalid("private directory is a symlink".into()));
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
pub fn private_store(path: &Path) -> Result<()> {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(Error::Invalid("private store is a symlink".into()));
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [
            path.to_path_buf(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            match std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}
/// Atomically publish a private file, refusing replacement unless explicitly requested.
pub fn write_secret(path: &Path, text: &str, replace: bool) -> Result<bool> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Invalid("secret has no parent".into()))?;
    let temporary = parent.join(format!(".{}.tmp", new_token(9)));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<bool> {
        let mut file = options.open(&temporary)?;
        file.write_all(format!("{text}\n").as_bytes())?;
        file.sync_all()?;
        drop(file);
        if replace {
            std::fs::rename(&temporary, path)?;
            Ok(true)
        } else {
            match std::fs::hard_link(&temporary, path) {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
                Err(e) => Err(e.into()),
            }
        }
    })();
    let _ = std::fs::remove_file(temporary);
    result
}
pub fn secret_file(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(value) => Ok(value.trim().into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let token = new_token(32);
            write_secret(path, &token, false)?;
            Ok(std::fs::read_to_string(path)?.trim().into())
        }
        Err(e) => Err(e.into()),
    }
}
#[derive(Clone, Debug)]
pub struct AppLayout {
    pub data_dir: PathBuf,
    pub server: PathBuf,
    pub server_store: PathBuf,
    pub workers: PathBuf,
    pub plugins: PathBuf,
    pub logs: PathBuf,
    pub worker_bases: PathBuf,
    pub compile_cache: PathBuf,
    pub owner_token: PathBuf,
    pub cookie_secret: PathBuf,
}
impl AppLayout {
    pub fn create(path: &Path) -> Result<Self> {
        if !path.is_absolute() {
            return Err(Error::Invalid("data directory must be absolute".into()));
        }
        if path.parent().is_none() {
            return Err(Error::Invalid("data directory cannot be root".into()));
        }
        // Refuse home/root before chmod, including aliases through symbolic links.
        std::fs::create_dir_all(path)?;
        let data_dir = std::fs::canonicalize(path)?;
        if data_dir.parent().is_none()
            || std::env::var_os("HOME")
                .and_then(|p| std::fs::canonicalize(p).ok())
                .as_ref()
                == Some(&data_dir)
        {
            return Err(Error::Invalid(
                "data directory must be an application's own directory".into(),
            ));
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::MetadataExt;
            let status = std::fs::read_to_string("/proc/self/status")?;
            let uid = status
                .lines()
                .find_map(|line| {
                    line.strip_prefix("Uid:")
                        .and_then(|line| line.split_whitespace().nth(1))
                        .and_then(|v| v.parse::<u32>().ok())
                })
                .ok_or_else(|| Error::Invalid("cannot determine effective uid".into()))?;
            if std::fs::metadata(&data_dir)?.uid() != uid {
                return Err(Error::Invalid(
                    "data directory belongs to another user".into(),
                ));
            }
        }
        private_directory(&data_dir)?;
        let layout = Self {
            server: data_dir.join("server"),
            server_store: data_dir.join("server/store.sqlite"),
            workers: data_dir.join("workers"),
            plugins: data_dir.join("plugins"),
            logs: data_dir.join("logs"),
            worker_bases: data_dir.join("cache/workers"),
            compile_cache: data_dir.join("cache/node"),
            owner_token: data_dir.join("owner-token"),
            cookie_secret: data_dir.join("server/cookie-secret"),
            data_dir,
        };
        for path in [
            &layout.server,
            &layout.workers,
            &layout.plugins,
            &layout.logs,
            &layout.data_dir.join("cache"),
            &layout.worker_bases,
            &layout.compile_cache,
        ] {
            private_directory(path)?;
        }
        private_store(&layout.server_store)?;
        Ok(layout)
    }
    pub fn worker_directory(&self, kind: &str, key: &str) -> Result<PathBuf> {
        safe_relative(kind)?;
        if key.is_empty() || key.contains('\0') {
            return Err(Error::Invalid("worker key is empty or contains NUL".into()));
        }
        if kind.contains('/') {
            return Err(Error::Invalid("worker kind is not one segment".into()));
        }
        let encoded = key
            .bytes()
            .map(|b| {
                if b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_' {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect::<String>();
        Ok(self.workers.join(kind).join(if encoded.len() > 160 {
            format!("sha256-{}", super::plugins::digest(key.as_bytes()))
        } else {
            encoded
        }))
    }
}
