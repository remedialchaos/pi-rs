use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use pi_rs_chord::{
    Error, Result,
    node::app::{private_directory, private_store},
    sqlite::{Options, SqliteDatabase},
    store::StoreEngine,
};

pub fn server_directory(data_dir: &Path) -> PathBuf {
    data_dir.join("server")
}

pub fn telemetry_store(data_dir: &Path) -> PathBuf {
    data_dir.join("telemetry.sqlite")
}

pub fn user_plugins_directory(data_dir: &Path) -> PathBuf {
    data_dir.join("plugins")
}

pub fn session_directory(data_dir: &Path, id: &str) -> Result<PathBuf> {
    let mut bytes = id.bytes();
    if id.len() > 128
        || !bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        || !bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(Error::Invalid(format!(
            "Invalid session id {}",
            serde_json::Value::from(id)
        )));
    }
    Ok(data_dir.join("sessions").join(id))
}

pub fn host_files(dir: &Path) -> PathBuf {
    dir.join("files")
}

pub fn host_store(dir: &Path) -> PathBuf {
    dir.join("store.sqlite")
}

pub async fn open_host_store(dir: &Path) -> Result<StoreEngine> {
    private_directory(dir)?;
    let path = host_store(dir);
    private_store(&path)?;
    let db = SqliteDatabase::open(path, Options::default()).await?;
    StoreEngine::open(db).await
}

pub fn assert_migrated(data_dir: &Path) -> Result<()> {
    let mut old = [
        "server.sqlite",
        "catalog.sqlite",
        "shares.sqlite",
        "subagents.sqlite",
    ]
    .iter()
    .any(|file| data_dir.join(file).exists());
    if !old {
        match fs::read_dir(data_dir.join("sessions")) {
            Ok(entries) => {
                for entry in entries {
                    if entry?.file_name().to_string_lossy().ends_with(".sqlite") {
                        old = true;
                        break;
                    }
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    if old {
        return Err(Error::Invalid(format!(
            "{} holds data in p2's old layout (server.sqlite, catalog.sqlite, shares.sqlite, subagents.sqlite or sessions/<id>.sqlite): stop p2 and run `npm run migrate-data`",
            data_dir.display()
        )));
    }
    Ok(())
}
