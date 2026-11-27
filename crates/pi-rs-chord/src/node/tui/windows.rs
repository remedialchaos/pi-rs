//! Hold the resolved directories against rename while opening a confirmed attachment.
use crate::{Error, Result};
use std::{
    fs::{File, OpenOptions},
    os::windows::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf, Prefix},
};

pub(super) fn open_local(path: &Path) -> Result<(File, Vec<File>)> {
    // FILE_SHARE_READ excludes delete/rename and writes. Open the reparse
    // point itself, so a junction or link swapped into any component is refused.
    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const REPARSE_POINT: u32 = 0x400;
    let invalid = || Error::Invalid("attachment must resolve to a local file".into());
    let components = path.components();
    if !matches!(components.clone().next(), Some(Component::Prefix(p)) if matches!(p.kind(),Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
    {
        return Err(invalid());
    }
    let mut directories = Vec::new();
    let mut current = PathBuf::new();
    for component in components {
        current.push(component.as_os_str());
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        let final_file = current == path;
        let flags = if final_file {
            OPEN_REPARSE_POINT
        } else {
            OPEN_REPARSE_POINT | BACKUP_SEMANTICS
        };
        let handle = OpenOptions::new()
            .read(true)
            .share_mode(1)
            .custom_flags(flags)
            .open(&current)?;
        let metadata = handle.metadata()?;
        if metadata.file_attributes() & REPARSE_POINT != 0 {
            return Err(invalid());
        }
        if final_file {
            return if metadata.is_file() {
                Ok((handle, directories))
            } else {
                Err(invalid())
            };
        }
        if !metadata.is_dir() {
            return Err(invalid());
        }
        directories.push(handle);
    }
    Err(invalid())
}
