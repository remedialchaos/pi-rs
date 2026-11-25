//! `locations` plugin (`p2.agent-locations`, `p2.location-move`, location tools & deferred mover).

pub mod server;
pub mod tool;
pub mod worker;

use std::{
    future::Future,
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use pi_rs_chord::{Error, Result, context::Context};
use pi_rs_durable::{
    DocToken,
    documents::{DocDefinition, DocumentSemantics, InitialDocument, LatestFork},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::{
    api::v1::{FacetKind, PluginRegistry},
    runtime::session_worker::Location,
};

/// Native server service key for the SSH/sandbox environment manager (published by `envs`).
pub const ENV_MANAGERS_SERVICE: &str = "p2.env-managers";

pub type BoxedFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// Server-side environment directory lister (implemented by `envs` and mockable in tests).
pub trait EnvDirectoryManager: Send + Sync {
    fn list_directory<'a>(
        &'a self,
        env: &'a str,
        path: &'a str,
    ) -> BoxedFuture<'a, DirectoryListing>;
}

/// Wrapper stored in `install_service` under [`ENV_MANAGERS_SERVICE`].
#[derive(Clone)]
pub struct EnvManagers {
    pub manager: Arc<dyn EnvDirectoryManager>,
}

impl EnvManagers {
    pub fn new(manager: Arc<dyn EnvDirectoryManager>) -> Self {
        Self { manager }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryListing {
    pub path: String,
    pub parent: Option<String>,
    pub directories: Vec<String>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "ok")]
pub enum EnvTestSummary {
    #[serde(rename = "true")]
    Ok { platform: String, arch: String },
    #[serde(rename = "false")]
    Err { error: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvInfo {
    pub name: String,
    #[serde(rename = "type")]
    pub env_type: String,
    pub host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tested: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocationMoveFailure {
    pub to: Location,
    pub error: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocationMoveState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<Location>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<LocationMoveFailure>,
}

pub fn location_move_doc() -> DocToken {
    Arc::new(DocDefinition {
        kind: "p2.location-move".into(),
        version: 1,
        semantics: DocumentSemantics::LatestConversation {
            fork: LatestFork::Initial,
        },
        initial: InitialDocument::Singleton(Arc::new(|| Ok(Map::new()))),
        migrate: None,
        checkpoint_when: Arc::new(|_, _, info| info.deltas_since_base >= 64),
    })
}

pub type LocateFuture<'a> = Pin<Box<dyn Future<Output = Result<Location>> + Send + 'a>>;

pub trait LocationServer: Send + Sync {
    fn envs<'a>(&'a self, context: &'a Context) -> BoxedFuture<'a, Vec<EnvInfo>>;
    fn directories<'a>(
        &'a self,
        env: Option<&'a str>,
        path: &'a str,
        context: &'a Context,
    ) -> BoxedFuture<'a, DirectoryListing>;
}

pub fn normalize_local_path(raw: &str) -> Result<PathBuf> {
    let trimmed = raw.trim();
    let expanded = if trimmed == "~" {
        std::env::home_dir()
            .ok_or_else(|| Error::Invalid("Cannot determine home directory".into()))?
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        std::env::home_dir()
            .ok_or_else(|| Error::Invalid("Cannot determine home directory".into()))?
            .join(rest)
    } else {
        PathBuf::from(trimmed)
    };
    let abs = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()?.join(expanded)
    };
    let mut normalized = PathBuf::new();
    for comp in abs.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

/// Lists subdirectories of `path` on this device (expanding `~`), matching `listLocalDirectory`.
pub async fn list_local_directory(path: &str) -> Result<DirectoryListing> {
    let absolute = normalize_local_path(path)?;
    let mut rd = tokio::fs::read_dir(&absolute).await?;
    let mut entry_count = 0usize;
    let mut directories = Vec::new();
    while let Some(entry) = rd.next_entry().await? {
        entry_count += 1;
        if entry_count > 10_000 {
            continue;
        }
        let Ok(file_type) = entry.file_type().await else {
            continue;
        };
        let entry_path = entry.path();
        let is_dir = if file_type.is_dir() {
            true
        } else if file_type.is_symlink() {
            tokio::fs::metadata(&entry_path)
                .await
                .is_ok_and(|meta| meta.is_dir())
        } else {
            false
        };
        if is_dir {
            directories.push(entry_path.to_string_lossy().into_owned());
        }
    }
    directories.sort();
    let truncated = entry_count > 10_000 || directories.len() > 1_000;
    directories.truncate(1_000);
    let abs_str = absolute.to_string_lossy().into_owned();
    let parent = absolute
        .parent()
        .filter(|p| *p != Path::new(&abs_str))
        .map(|p| p.to_string_lossy().into_owned());
    Ok(DirectoryListing {
        path: abs_str,
        parent,
        directories,
        truncated,
    })
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

/// Canonical SHA-256 prefix (12 hex chars) of an environment config, matching upstream `envRevision`.
pub fn env_revision(config: &Value) -> String {
    let serialized = serde_json::to_string(&canonicalize_json(config)).unwrap_or_default();
    let digest = Sha256::digest(serialized.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex.chars().take(12).collect()
}

/// Returns `true` when `name` is configured as a `gondolin` sandbox.
pub fn is_sandbox(configs: &Value, name: &str) -> bool {
    configs
        .as_object()
        .and_then(|map| map.get(name))
        .and_then(|cfg| cfg.get("type"))
        .and_then(Value::as_str)
        == Some("gondolin")
}

/// Formats `tested` info from `EnvService.tests` if its revision matches `config`.
pub fn tested_summary(config: &Value, test_entry: Option<&Value>) -> Option<Value> {
    let entry = test_entry?;
    let rev = entry.get("revision").and_then(Value::as_str)?;
    if rev != env_revision(config) {
        return None;
    }
    let test = entry.get("test")?;
    if test.get("ok").and_then(Value::as_bool) == Some(true) {
        let platform = test.get("platform")?.as_str()?;
        let arch = test.get("arch")?.as_str()?;
        Some(json!({ "ok": true, "platform": platform, "arch": arch }))
    } else {
        let error = test.get("error")?.as_str()?;
        Some(json!({ "ok": false, "error": error }))
    }
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    registry.register("locations", FacetKind::Server, server::server_facet()?)?;
    registry.register("locations", FacetKind::Worker, worker::worker_facet()?)?;
    Ok(())
}
