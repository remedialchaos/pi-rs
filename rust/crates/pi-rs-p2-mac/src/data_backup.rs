use crate::{Error, Result, private_directory, server_config::Preferences};
use std::{
    ffi::OsString,
    fs, io,
    path::{Component, Path, PathBuf},
};

pub const RETAINED: usize = 3;

pub fn before_upgrade(
    data: &Path,
    build: &str,
    preferences: &mut dyn Preferences,
) -> Result<Option<String>> {
    let key = format!("lastBuild:{}", data.display());
    let previous = preferences.get(&key);
    preferences.set(&key, Some(build))?;
    let Some(previous) = previous.filter(|prev| prev != build && data.is_dir()) else {
        return Ok(None);
    };
    let target = backup(data, &previous)?;
    Ok(Some(format!(
        "data from build {previous} backed up to {}",
        target.display()
    )))
}

fn build_component(build: &str) -> bool {
    !build.is_empty()
        && build
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        && matches!(
            Path::new(build).components().next(),
            Some(Component::Normal(_))
        )
}

pub fn backup(data: &Path, previous: &str) -> Result<PathBuf> {
    if !build_component(previous) {
        return Err(Error::Protocol("invalid backup build identifier".into()));
    }
    let mut name = OsString::from(
        data.file_name()
            .ok_or_else(|| Error::Protocol("data directory has no file name".into()))?,
    );
    name.push(".backups");
    let root = data.with_file_name(name);
    private_directory(&root)?;
    if !fs::symlink_metadata(&root)?.is_dir() {
        return Err(Error::Protocol(
            "backup root must be a directory, not a link".into(),
        ));
    }
    let stage = tempfile::tempdir_in(&root)?;
    let snapshot = stage.path().join("snapshot");
    copy_directory(data, &snapshot)?;
    let target = root.join(previous);
    let old = stage.path().join("previous");
    let had_old = match fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.is_dir() => {
            fs::rename(&target, &old)?;
            true
        }
        Ok(_) => return Err(Error::Protocol("backup target is not a directory".into())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.into()),
    };
    if let Err(error) = fs::rename(&snapshot, &target) {
        if had_old && let Err(restore) = fs::rename(&old, &target) {
            let path = stage.keep();
            return Err(Error::Protocol(format!(
                "backup failed: {error}; could not restore {restore}; prior copy remains at {}",
                path.join("previous").display()
            )));
        }
        return Err(error.into());
    }
    drop(stage);
    let mut copies = Vec::new();
    for item in fs::read_dir(&root)? {
        let item = item?;
        if !item.file_type()?.is_dir()
            || !item.file_name().to_str().is_some_and(build_component)
            || item.file_name().as_encoded_bytes().starts_with(b".")
        {
            continue;
        }
        let meta = item.metadata()?;
        copies.push((
            meta.created().or_else(|_| meta.modified()).ok(),
            item.path(),
        ));
    }
    copies.sort_unstable_by(|a, b| b.cmp(a));
    for (_, entry) in copies.into_iter().skip(RETAINED) {
        fs::remove_dir_all(entry)?;
    }
    Ok(target)
}

#[cfg(target_os = "macos")]
fn copy_directory(from: &Path, to: &Path) -> io::Result<()> {
    let status = std::process::Command::new("/bin/cp")
        .arg("-cRp")
        .arg(from)
        .arg(to)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("cp exited with {status}")))
    }
}

#[cfg(not(target_os = "macos"))]
fn copy_directory(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_directory(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &target)?;
            let modified = entry.metadata()?.modified()?;
            fs::File::open(&target)?.set_times(fs::FileTimes::new().set_modified(modified))?;
        } else if kind.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(entry.path())?, target)?;
            #[cfg(windows)]
            if entry.path().is_dir() {
                std::os::windows::fs::symlink_dir(fs::read_link(entry.path())?, target)?;
            } else {
                std::os::windows::fs::symlink_file(fs::read_link(entry.path())?, target)?;
            }
        }
    }
    fs::set_permissions(to, fs::metadata(from)?.permissions())
}
