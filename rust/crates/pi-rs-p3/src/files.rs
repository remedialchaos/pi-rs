//! The base's files: the data directory and the stores made private (directories 0700, files 0600,
//! also when they existed looser: other local users must not read a store's sign-ins, nor replace
//! it), and private files written in one step.
//!
//! Directory and store mode handling is `pi-rs-chord`'s (`node::app::private_directory` and
//! `node::app::private_store`, the platform host's own file rules); this module adds the data
//! directory's own checks and the one-step private write.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

/// What a data directory's check refused, or a private file's write failed with.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error("p3: no home directory to place ~/.pi/p3 in")]
    NoHome,
    #[error("p3: the data directory {path} must be a directory of p3's own")]
    NotOwn { path: PathBuf },
    #[error("p3: the data directory {path} is not writable: {source}")]
    NotWritable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The user's home directory.
pub fn home_dir() -> Result<PathBuf, FileError> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    match home {
        Some(home) if !home.is_empty() => Ok(PathBuf::from(home)),
        _ => Err(FileError::NoHome),
    }
}

/// The default Pi's data directory: `~/.pi/p3`.
pub fn default_data_dir() -> Result<PathBuf, FileError> {
    Ok(home_dir()?.join(".pi").join("p3"))
}

/// The real path of the data directory `path`, made private (0700) and made when missing; refused
/// when it is the home directory, a file system's root, or not writable by this user.
pub fn open_data_dir(path: &Path) -> Result<PathBuf, FileError> {
    // Never tighten the mode of a directory before knowing it is p3's own: `/` and `~` are refused.
    if !path.exists() {
        pi_rs_chord::node::app::private_directory(path).map_err(chord_file_error)?;
    }
    let data_dir = fs::canonicalize(path)?;
    let home = fs::canonicalize(home_dir()?).ok();
    if home.as_deref() == Some(data_dir.as_path()) || data_dir.parent().is_none() {
        return Err(FileError::NotOwn { path: data_dir });
    }
    // No safe standard API exposes getuid; writability is the property that matters, and a directory
    // of another user is not writable by this one.
    let probe = data_dir.join(format!(".p3-write-{}", uuid::Uuid::new_v4()));
    match fs::write(&probe, b"") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
        }
        Err(source) => {
            return Err(FileError::NotWritable {
                path: data_dir,
                source,
            });
        }
    }
    set_private_directory(&data_dir)?;
    Ok(data_dir)
}

/// Makes `dir` (and missing parents) with mode 0700, and sets 0700 on it when it existed.
pub fn private_directory(dir: &Path) -> Result<(), FileError> {
    pi_rs_chord::node::app::private_directory(dir).map_err(chord_file_error)
}

/// Makes the store file `path` private before a store opens it: created empty with mode 0600 when
/// missing, else set to 0600 with the journal files it has.
pub fn private_store(path: &Path) -> Result<(), FileError> {
    pi_rs_chord::node::app::private_store(path).map_err(chord_file_error)
}

fn chord_file_error(error: pi_rs_chord::Error) -> FileError {
    FileError::Io(std::io::Error::other(error.to_string()))
}

/// Writes `text` to `path` with mode 0600 in one step: a temporary file of the same directory,
/// synced, renamed.
pub fn write_private(path: &Path, text: &str) -> Result<(), FileError> {
    let parent = path
        .parent()
        .ok_or_else(|| FileError::Io(std::io::Error::other("private file has no parent")))?;
    let temporary = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<(), FileError> {
        let mut file = options.open(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn set_private_directory(path: &Path) -> Result<(), FileError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_data_dir_makes_a_missing_directory_private() {
        let root = tempfile::tempdir().expect("temp dir");
        let path = root.path().join("pi");
        let opened = open_data_dir(&path).expect("data dir");
        assert_eq!(opened, fs::canonicalize(&path).expect("canonical"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&opened)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700, "the data directory is private");
        }
    }

    #[test]
    fn open_data_dir_refuses_a_file_systems_root_and_the_home_directory() {
        let root = Path::new("/");
        assert!(matches!(open_data_dir(root), Err(FileError::NotOwn { .. })));
        let home = home_dir().expect("home");
        assert!(
            matches!(open_data_dir(&home), Err(FileError::NotOwn { .. })),
            "the home directory is no Pi's"
        );
    }

    #[test]
    fn a_private_write_leaves_only_the_target_with_mode_0600() {
        let root = tempfile::tempdir().expect("temp dir");
        let path = root.path().join("pi.json");
        write_private(&path, "{\"port\":8789}").expect("write");
        assert_eq!(fs::read_to_string(&path).expect("read"), "{\"port\":8789}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).expect("metadata").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the file is private");
        }
        let leftovers = fs::read_dir(root.path())
            .expect("listing")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name() != "pi.json")
            .count();
        assert_eq!(leftovers, 0, "no temporary file is left behind");
    }

    #[test]
    fn a_private_write_replaces_the_previous_text_whole() {
        let root = tempfile::tempdir().expect("temp dir");
        let path = root.path().join("owner-token");
        write_private(&path, "first").expect("write");
        write_private(&path, "second").expect("rewrite");
        assert_eq!(fs::read_to_string(&path).expect("read"), "second");
    }
}
