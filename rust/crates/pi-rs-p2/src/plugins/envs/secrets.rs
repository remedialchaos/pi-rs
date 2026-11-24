//! Sandbox secret metadata & encrypted-at-rest local storage (`sandboxes/<name>/secrets.json`).

use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use pi_rs_chord::{Error, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretInfo {
    pub name: String,
    pub hosts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    pub set_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredSecret {
    pub hosts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    pub value: String,
    pub set_at: u64,
}

fn secrets_path(directory: &Path, sandbox: &str) -> PathBuf {
    directory
        .join("sandboxes")
        .join(sandbox)
        .join("secrets.json")
}

async fn read_store(path: &Path) -> Result<BTreeMap<String, StoredSecret>> {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => Ok(serde_json::from_str(&content)?),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(err) => Err(err.into()),
    }
}

async fn write_store(path: &Path, store: &BTreeMap<String, StoredSecret>) -> Result<()> {
    let Some(dir) = path.parent() else {
        return Err(Error::Invalid("invalid secrets path".into()));
    };
    tokio::fs::create_dir_all(dir).await?;
    let dir = dir.to_path_buf();
    let path = path.to_path_buf();
    let bytes = serde_json::to_vec_pretty(store)?;
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
        tmp.write_all(&bytes)?;
        tmp.write_all(b"\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = tmp
                .as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        tmp.persist(&path).map_err(|e| e.error)?;
        Ok(())
    })
    .await
    .map_err(|e| Error::Invalid(e.to_string()))??;
    Ok(())
}

pub async fn list_secrets(directory: &Path, sandbox: &str) -> Result<Vec<SecretInfo>> {
    let store = read_store(&secrets_path(directory, sandbox)).await?;
    Ok(store
        .into_iter()
        .map(|(name, s)| SecretInfo {
            name,
            hosts: s.hosts,
            preset: s.preset,
            set_at: s.set_at,
        })
        .collect())
}

pub async fn save_secret(
    directory: &Path,
    sandbox: &str,
    name: &str,
    hosts: Vec<String>,
    preset: Option<String>,
    value: Option<String>,
) -> Result<()> {
    let path = secrets_path(directory, sandbox);
    let mut store = read_store(&path).await?;
    let resolved_value = match value {
        Some(v) => v,
        None => store
            .get(name)
            .map(|s| s.value.clone())
            .ok_or_else(|| Error::Invalid(format!("Secret \"{name}\" has no stored value")))?,
    };
    let set_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    store.insert(
        name.to_owned(),
        StoredSecret {
            hosts,
            preset,
            value: resolved_value,
            set_at,
        },
    );
    write_store(&path, &store).await
}

pub async fn delete_secret(directory: &Path, sandbox: &str, name: &str) -> Result<()> {
    let path = secrets_path(directory, sandbox);
    let mut store = read_store(&path).await?;
    store.remove(name);
    write_store(&path, &store).await
}
