//! Sandbox secret metadata & local storage (`<data>/sandbox-secrets.json`) with transparent
//! fallback & migration from legacy `<data>/sandboxes/<name>/secrets.json`.

use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    sync::LazyLock,
    time::{SystemTime, UNIX_EPOCH},
};

use pi_rs_chord::{Error, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

static SECRET_NAME_RE: LazyLock<Regex> = LazyLock::new(|| {
    // INFALLIBLE: literal anchored regex.
    Regex::new(r"^[A-Z_][A-Z0-9_]{0,63}$").expect("secret name regex")
});

static HOST_RE: LazyLock<Regex> = LazyLock::new(|| {
    // INFALLIBLE: literal anchored regex.
    Regex::new(r"^[A-Za-z0-9*][A-Za-z0-9.*-]*$").expect("secret host regex")
});

struct SecretPreset {
    id: &'static str,
    aliases: &'static [&'static str],
    git_hosts: &'static [&'static str],
}

const PRESETS: &[SecretPreset] = &[
    SecretPreset {
        id: "github",
        aliases: &["GH_TOKEN"],
        git_hosts: &["github.com"],
    },
    SecretPreset {
        id: "radius",
        aliases: &[],
        git_hosts: &[],
    },
    SecretPreset {
        id: "npm",
        aliases: &[],
        git_hosts: &[],
    },
    SecretPreset {
        id: "custom",
        aliases: &[],
        git_hosts: &[],
    },
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretInfo {
    pub name: String,
    pub hosts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    pub set_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretMeta {
    pub name: String,
    pub hosts: Vec<String>,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_hosts: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredSecret {
    pub hosts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    pub value: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(alias = "set_at")]
    pub set_at: u64,
}

fn default_version() -> String {
    Uuid::new_v4().simple().to_string()[..12].to_owned()
}

type SecretsFile = BTreeMap<String, BTreeMap<String, StoredSecret>>;

pub fn secrets_path(directory: &Path) -> PathBuf {
    directory.join("sandbox-secrets.json")
}

fn legacy_secrets_path(directory: &Path, sandbox: &str) -> PathBuf {
    directory
        .join("sandboxes")
        .join(sandbox)
        .join("secrets.json")
}

async fn read_all_secrets(directory: &Path) -> Result<SecretsFile> {
    let path = secrets_path(directory);
    let mut file_map: SecretsFile = match tokio::fs::read_to_string(&path).await {
        Ok(content) if !content.trim().is_empty() => serde_json::from_str(&content)
            .map_err(|_| Error::Invalid("Invalid sandbox-secrets.json".into()))?,
        Ok(_) => BTreeMap::new(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(err) => return Err(err.into()),
    };

    // Fallback/merge legacy per-sandbox files (`<data>/sandboxes/<name>/secrets.json`) if present.
    let legacy_root = directory.join("sandboxes");
    if let Ok(mut entries) = tokio::fs::read_dir(&legacy_root).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let sandbox_name = entry.file_name().to_string_lossy().into_owned();
            let legacy_file = legacy_secrets_path(directory, &sandbox_name);
            if let Ok(text) = tokio::fs::read_to_string(&legacy_file).await
                && let Ok(parsed) = serde_json::from_str::<BTreeMap<String, StoredSecret>>(&text)
                && !parsed.is_empty()
            {
                let target = file_map.entry(sandbox_name).or_default();
                for (name, secret) in parsed {
                    target.entry(name).or_insert(secret);
                }
            }
        }
    }

    Ok(file_map)
}

async fn write_all_secrets(directory: &Path, store: &SecretsFile) -> Result<()> {
    tokio::fs::create_dir_all(directory).await?;
    let dir = directory.to_path_buf();
    let path = secrets_path(directory);
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
    let all = read_all_secrets(directory).await?;
    let store = all.get(sandbox).cloned().unwrap_or_default();
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

pub async fn sandbox_secrets(
    directory: &Path,
    sandbox: &str,
) -> Result<(Vec<SecretMeta>, BTreeMap<String, String>)> {
    let all = read_all_secrets(directory).await?;
    let store = all.get(sandbox).cloned().unwrap_or_default();
    let mut meta = Vec::with_capacity(store.len());
    let mut values = BTreeMap::new();
    for (name, secret) in store {
        let preset = secret
            .preset
            .as_deref()
            .and_then(|id| PRESETS.iter().find(|p| p.id == id));
        meta.push(SecretMeta {
            name: name.clone(),
            hosts: secret.hosts,
            version: secret.version,
            aliases: preset
                .filter(|p| !p.aliases.is_empty())
                .map(|p| p.aliases.iter().map(|s| (*s).to_owned()).collect()),
            git_hosts: preset
                .filter(|p| !p.git_hosts.is_empty())
                .map(|p| p.git_hosts.iter().map(|s| (*s).to_owned()).collect()),
        });
        values.insert(name, secret.value);
    }
    Ok((meta, values))
}

pub async fn save_secret(
    directory: &Path,
    sandbox: &str,
    name: &str,
    hosts: Vec<String>,
    preset: Option<String>,
    value: Option<String>,
    allow_hosts: Option<&[String]>,
) -> Result<()> {
    if !SECRET_NAME_RE.is_match(name) {
        return Err(Error::Invalid(
            "Secret names are environment variables: A-Z, 0-9 and _".into(),
        ));
    }
    if hosts.is_empty() {
        return Err(Error::Invalid("A secret needs at least one host".into()));
    }
    for host in &hosts {
        if !HOST_RE.is_match(host) {
            return Err(Error::Invalid(format!("Invalid host: {host}")));
        }
        if let Some(allowed) = allow_hosts
            && !allowed.iter().any(|candidate| candidate == host)
        {
            return Err(Error::Invalid(format!(
                "{host} is not an allowed host of sandbox {sandbox}"
            )));
        }
    }
    if let Some(ref val) = value {
        if val.is_empty() || val.chars().count() > 16_384 {
            return Err(Error::Invalid(
                "A secret's value must have 1 to 16384 characters".into(),
            ));
        }
        if val.contains(['\r', '\n']) {
            return Err(Error::Invalid("A secret's value is one line".into()));
        }
    }

    let mut all = read_all_secrets(directory).await?;
    let own = all.entry(sandbox.to_owned()).or_default();
    let current = own.get(name).cloned();
    let resolved_value = match value.as_ref() {
        Some(v) => v.clone(),
        None => current
            .as_ref()
            .map(|s| s.value.clone())
            .ok_or_else(|| Error::Invalid(format!("Secret {name} needs a value")))?,
    };
    let set_at = if value.is_none() {
        current.as_ref().map_or_else(now_millis, |s| s.set_at)
    } else {
        now_millis()
    };
    let resolved_preset = preset.or_else(|| current.and_then(|s| s.preset));
    own.insert(
        name.to_owned(),
        StoredSecret {
            hosts,
            preset: resolved_preset,
            value: resolved_value,
            version: default_version(),
            set_at,
        },
    );
    write_all_secrets(directory, &all).await
}

pub async fn delete_secret(directory: &Path, sandbox: &str, name: Option<&str>) -> Result<()> {
    let mut all = read_all_secrets(directory).await?;
    match name {
        None => {
            all.remove(sandbox);
        }
        Some(secret_name) => {
            if let Some(own) = all.get_mut(sandbox) {
                own.remove(secret_name);
                if own.is_empty() {
                    all.remove(sandbox);
                }
            }
        }
    }
    write_all_secrets(directory, &all).await
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
