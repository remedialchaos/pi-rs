//! `envs.json` configuration schema, validation, and atomic persistence.

use std::{
    collections::{BTreeMap, HashSet},
    io::Write,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use pi_rs_chord::{Error, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub const DEFAULT_DISK_GB: u64 = 32;

static ENV_NAME_RE: LazyLock<Regex> = LazyLock::new(|| {
    // INFALLIBLE: literal anchored character class.
    Regex::new(r"^[a-z0-9][a-z0-9_-]{0,31}$").expect("env name regex")
});

static IMAGE_RE: LazyLock<Regex> = LazyLock::new(|| {
    // INFALLIBLE: literal anchored character class.
    Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._:/@-]*$").expect("image regex")
});

static ALLOW_HOST_RE: LazyLock<Regex> = LazyLock::new(|| {
    // INFALLIBLE: literal anchored character class.
    Regex::new(r"^[A-Za-z0-9*][A-Za-z0-9.*-]*$").expect("allow host regex")
});

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshEnvConfig {
    pub host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login_shell: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell_env: Option<BTreeMap<String, String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxMount {
    pub host: String,
    pub guest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readonly: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GondolinEnvConfig {
    pub host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mounts: Option<Vec<SandboxMount>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_hosts: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_gb: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum EnvConfig {
    Ssh(SshEnvConfig),
    Gondolin(GondolinEnvConfig),
}

pub type EnvsConfig = BTreeMap<String, EnvConfig>;

const RESERVED_SANDBOX_PATHS: [&str; 2] = ["/opt/pi-env", "/.p2-vfs"];

pub fn is_valid_env_name(name: &str) -> bool {
    ENV_NAME_RE.is_match(name)
}

fn has_control(value: &str) -> bool {
    value
        .chars()
        .any(|c| (c as u32) < 0x20 || (c as u32) == 0x7f)
}

fn ssh_field(value: Option<&str>) -> bool {
    match value {
        None => true,
        Some(v) => !v.starts_with('-') && !v.chars().any(char::is_whitespace) && !has_control(v),
    }
}

fn guest_path(path: &str) -> bool {
    path.starts_with('/')
        && path != "/"
        && !has_control(path)
        && path
            .split('/')
            .skip(1)
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

pub fn validate_env(config: &EnvConfig) -> Result<()> {
    match config {
        EnvConfig::Gondolin(cfg) => {
            if !is_valid_env_name(&cfg.host) {
                return Err(Error::Invalid("Invalid host environment".into()));
            }
            if let Some(image) = &cfg.image
                && !IMAGE_RE.is_match(image)
            {
                return Err(Error::Invalid("Invalid image".into()));
            }
            if let Some(cpus) = cfg.cpus
                && !(1..=64).contains(&cpus)
            {
                return Err(Error::Invalid("Invalid cpus".into()));
            }
            if let Some(mem) = cfg.memory_mb
                && !(256..=262_144).contains(&mem)
            {
                return Err(Error::Invalid("Invalid memoryMb".into()));
            }
            if let Some(disk) = cfg.disk_gb
                && !(1..=4096).contains(&disk)
            {
                return Err(Error::Invalid("Invalid diskGb".into()));
            }
            let mut guests = HashSet::new();
            for mount in cfg.mounts.as_deref().unwrap_or(&[]) {
                let valid_host = mount.host == "~"
                    || mount.host.starts_with("~/")
                    || mount.host.starts_with('/');
                if !valid_host || has_control(&mount.host) {
                    return Err(Error::Invalid(format!("Invalid folder: {}", mount.host)));
                }
                if !guest_path(&mount.guest) {
                    return Err(Error::Invalid(format!(
                        "Invalid path in the sandbox: {}",
                        mount.guest
                    )));
                }
                if RESERVED_SANDBOX_PATHS.iter().any(|reserved| {
                    mount.guest == *reserved
                        || reserved.starts_with(&format!("{}/", mount.guest))
                        || mount.guest.starts_with(&format!("{reserved}/"))
                }) {
                    return Err(Error::Invalid(format!(
                        "{} is used by p2 in the sandbox ({})",
                        mount.guest,
                        RESERVED_SANDBOX_PATHS.join(", ")
                    )));
                }
                if mount.readonly != Some(true)
                    && (mount.host == "~"
                        || mount.host == "/"
                        || mount.host == "~/.pi"
                        || mount.host.starts_with("~/.pi/"))
                {
                    return Err(Error::Invalid(format!(
                        "A writable folder must not contain or be inside ~/.pi: {}",
                        mount.host
                    )));
                }
                if !guests.insert(mount.guest.clone()) {
                    return Err(Error::Invalid(format!("Two folders at {}", mount.guest)));
                }
            }
            for pattern in cfg.allow_hosts.as_deref().unwrap_or(&[]) {
                if !ALLOW_HOST_RE.is_match(pattern) {
                    return Err(Error::Invalid(format!("Invalid allowed host: {pattern}")));
                }
            }
            Ok(())
        }
        EnvConfig::Ssh(cfg) => {
            if cfg.host.is_empty() || cfg.host.len() > 255 {
                return Err(Error::Invalid("Invalid host".into()));
            }
            if cfg.port == Some(0) {
                return Err(Error::Invalid("Invalid port".into()));
            }
            if !ssh_field(Some(&cfg.host)) || !ssh_field(cfg.user.as_deref()) {
                return Err(Error::Invalid("Invalid host or user".into()));
            }
            if has_control(cfg.identity_file.as_deref().unwrap_or(""))
                || cfg
                    .identity_file
                    .as_deref()
                    .is_some_and(|s| s.contains('"'))
            {
                return Err(Error::Invalid("Invalid identity file".into()));
            }
            Ok(())
        }
    }
}

fn check_hosts(envs: &EnvsConfig) -> Result<()> {
    for (name, config) in envs {
        if let EnvConfig::Gondolin(cfg) = config
            && !matches!(envs.get(&cfg.host), Some(EnvConfig::Ssh(_)))
        {
            return Err(Error::Invalid(format!(
                "Sandbox {name} runs on \"{}\", which is not an SSH environment",
                cfg.host
            )));
        }
    }
    Ok(())
}

pub fn sandboxes_on<'a>(envs: &'a EnvsConfig, host: &str) -> Vec<(&'a str, &'a GondolinEnvConfig)> {
    envs.iter()
        .filter_map(|(name, cfg)| match cfg {
            EnvConfig::Gondolin(g) if g.host == host => Some((name.as_str(), g)),
            _ => None,
        })
        .collect()
}

pub fn is_sandbox(envs: &EnvsConfig, name: &str) -> bool {
    matches!(envs.get(name), Some(EnvConfig::Gondolin(_)))
}

fn canonicalize_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for key in keys {
                out.insert(key.clone(), canonicalize_json(&map[key]));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

pub fn env_revision(config: &EnvConfig) -> String {
    let val = serde_json::to_value(config).unwrap_or(Value::Null);
    let serialized = serde_json::to_string(&canonicalize_json(&val)).unwrap_or_default();
    let digest = Sha256::digest(serialized.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex.chars().take(12).collect()
}

pub fn parse_envs(content: &str) -> Result<EnvsConfig> {
    let trimmed = content.strip_prefix('\u{feff}').unwrap_or(content);
    let value: Value = serde_json::from_str(trimmed)
        .map_err(|_| Error::Invalid("Invalid envs.json syntax".into()))?;
    let Some(obj) = value.as_object() else {
        return Err(Error::Invalid("Invalid envs.json configuration".into()));
    };
    let mut envs = BTreeMap::new();
    for (name, raw_cfg) in obj {
        if !is_valid_env_name(name) {
            return Err(Error::Invalid(format!("Invalid environment name: {name}")));
        }
        let cfg: EnvConfig = serde_json::from_value(raw_cfg.clone())
            .map_err(|_| Error::Invalid("Invalid environment configuration".into()))?;
        validate_env(&cfg)?;
        envs.insert(name.clone(), cfg);
    }
    check_hosts(&envs)?;
    Ok(envs)
}

pub fn envs_path(directory: &Path) -> PathBuf {
    directory.join("envs.json")
}

pub async fn load_envs(directory: &Path) -> Result<EnvsConfig> {
    let path = envs_path(directory);
    match tokio::fs::read_to_string(&path).await {
        Ok(content) => parse_envs(&content),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(err) => Err(err.into()),
    }
}

pub async fn write_envs(directory: &Path, envs: &EnvsConfig) -> Result<()> {
    check_hosts(envs)?;
    tokio::fs::create_dir_all(directory).await?;
    let path = envs_path(directory);
    let dir = directory.to_path_buf();
    let bytes = serde_json::to_vec_pretty(envs)?;
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
        tmp.write_all(&bytes)?;
        tmp.write_all(b"\n")?;
        tmp.persist(&path).map_err(|e| e.error)?;
        Ok(())
    })
    .await
    .map_err(|e| Error::Invalid(e.to_string()))??;
    Ok(())
}

pub async fn save_env(directory: &Path, name: &str, config: EnvConfig) -> Result<EnvsConfig> {
    if !is_valid_env_name(name) {
        return Err(Error::Invalid(format!("Invalid environment name: {name}")));
    }
    validate_env(&config)?;
    let mut envs = load_envs(directory).await?;
    envs.insert(name.to_owned(), config);
    write_envs(directory, &envs).await?;
    Ok(envs)
}

pub async fn delete_env(directory: &Path, name: &str) -> Result<EnvsConfig> {
    let mut envs = load_envs(directory).await?;
    if envs.remove(name).is_none() {
        return Err(Error::Invalid(format!(
            "Environment \"{name}\" is not configured"
        )));
    }
    if let Some((sb, _)) = sandboxes_on(&envs, name).first() {
        return Err(Error::Invalid(format!(
            "Sandbox {sb} runs on \"{name}\"; delete or move it first"
        )));
    }
    write_envs(directory, &envs).await?;
    Ok(envs)
}
