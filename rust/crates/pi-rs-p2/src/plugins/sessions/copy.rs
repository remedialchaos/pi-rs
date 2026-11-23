use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, ErrorKind},
    path::Path,
};

use pi_rs_chord::{Error, Result};

fn directory(path: &Path, recursive: bool) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(recursive);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

pub(super) async fn copy(from: &Path, to: &Path) -> Result<()> {
    let (from, to) = (from.to_owned(), to.to_owned());
    tokio::task::spawn_blocking(move || {
        let parent = to
            .parent()
            .ok_or_else(|| Error::Invalid("Missing copy directory".into()))?;
        directory(parent, true)?;
        copy_tree(&from, &to, false)?;
        Ok(())
    })
    .await
    .map_err(|error| Error::Invalid(error.to_string()))?
}

fn copy_tree(from: &Path, to: &Path, blob: bool) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(from) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.is_dir() {
        directory(to, false)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            let name = entry.file_name();
            if name != ".uploads" {
                copy_tree(&entry.path(), &to.join(&name), blob || name == "blobs")?;
            }
        }
    } else if metadata.is_file() && !(blob && fs::hard_link(from, to).is_ok()) {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        io::copy(&mut File::open(from)?, &mut options.open(to)?)?;
    }
    Ok(())
}

pub(super) async fn remove(path: &Path) -> Result<()> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
