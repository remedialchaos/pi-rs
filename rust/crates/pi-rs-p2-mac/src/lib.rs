#![deny(unsafe_code)]

pub mod appearance;
pub mod brain;
pub mod brain_directory;
pub mod data_backup;
pub mod dev_control;
pub mod diagnostics;
pub mod host_bridge;
pub mod local_sign_in;
pub mod login_environment;
pub mod oauth_loopback;
pub mod pi_glyph;
pub mod radius_account;
pub mod server_config;
pub mod server_controller;
pub mod server_scan;
pub mod updater;
pub mod view_cookie;

#[cfg(target_os = "macos")]
pub mod macos;

use std::{
    fs,
    io::{self, Write},
    path::Path,
    time::Duration,
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    HttpTransport(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Url(#[from] url::ParseError),
    #[error("{0}")]
    Protocol(String),
    #[error("Signed out of Radius: {0}")]
    SignedOut(String),
    #[error("Pi is not enabled for this Radius account")]
    NotEnabled,
    #[error("Radius answered {0}: {1}")]
    Radius(u16, String),
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn http(timeout: Duration) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(timeout)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

pub(crate) fn private_directory(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

pub(crate) fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let directory = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    private_directory(directory)?;
    let mut temp = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer_pretty(&mut temp, value)?;
    temp.write_all(b"\n")?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}
