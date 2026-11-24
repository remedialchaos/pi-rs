//! Resource observer for `settings.json`, `models.json`, and `auth.json`.

use std::{
    collections::HashSet,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use pi_rs_ai::{
    auth::{CredentialInfo, CredentialStore, FileCredentialStore},
    models_store::InMemoryModelsStore,
    registry::{Models, ModelsOptions},
};
use pi_rs_chord::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::watch;

/// Native service key under which `ModelResources` is published via `install_service`.
pub const MODEL_RESOURCES_SERVICE: &str = "p2.model-resources";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub provider: String,
    pub model_id: String,
    pub name: String,
    pub thinking_levels: Vec<String>,
    pub context_window: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelChoice {
    pub provider: String,
    pub model_id: String,
    pub thinking_level: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceError {
    pub resource: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginMethodInfo {
    #[serde(rename = "type")]
    pub kind: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSummary {
    pub id: String,
    pub name: String,
    pub auth_methods: Vec<String>,
    pub login_methods: Vec<LoginMethodInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceSnapshot {
    pub version: u64,
    pub settings: Value,
    pub credentials: Vec<CredentialInfo>,
    pub providers: Vec<ProviderSummary>,
    pub models: Vec<ModelInfo>,
    pub errors: Vec<ResourceError>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSummary {
    pub models: Vec<ModelInfo>,
    pub default_model: Option<ModelChoice>,
    pub errors: Vec<String>,
}

pub struct ModelResources {
    pub directory: PathBuf,
    pub fake: bool,
    pub credentials: FileCredentialStore,
    pub models: Models,
    settings: RwLock<Value>,
    credentials_cache: RwLock<Vec<CredentialInfo>>,
    errors: RwLock<Vec<ResourceError>>,
    revision: AtomicU64,
    changes: watch::Sender<u64>,
}

impl ModelResources {
    pub async fn open(directory: PathBuf, fake: bool) -> Result<Arc<Self>> {
        let credentials = FileCredentialStore::new(directory.join("auth.json"));
        let models = if fake {
            Models::default()
        } else {
            Models::builtin(ModelsOptions {
                credentials: Arc::new(credentials.clone()),
                store: Arc::new(InMemoryModelsStore::default()),
                ..Default::default()
            })
            .map_err(|e| Error::Invalid(e.to_string()))?
        };
        let (changes, _) = watch::channel(0u64);
        let resources = Arc::new(Self {
            directory,
            fake,
            credentials,
            models,
            settings: RwLock::new(json!({})),
            credentials_cache: RwLock::new(Vec::new()),
            errors: RwLock::new(Vec::new()),
            revision: AtomicU64::new(0),
            changes,
        });
        resources.reload().await?;
        Ok(resources)
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    pub fn settings(&self) -> Value {
        self.settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub async fn reload(&self) -> Result<()> {
        let mut errors = Vec::new();

        match read_json_object(&self.directory.join("settings.json")).await {
            Ok(val) => {
                *self.settings.write().unwrap_or_else(|e| e.into_inner()) = val;
            }
            Err(msg) => {
                errors.push(ResourceError {
                    resource: "settings".into(),
                    message: msg,
                });
            }
        }

        let models_path = self.directory.join("models.json");
        if tokio::fs::try_exists(&models_path).await.unwrap_or(false)
            && let Err(msg) = read_json_object(&models_path).await
        {
            errors.push(ResourceError {
                resource: "models".into(),
                message: msg,
            });
        }

        match self.credentials.list(&Default::default()).await {
            Ok(list) => {
                *self
                    .credentials_cache
                    .write()
                    .unwrap_or_else(|e| e.into_inner()) = list;
            }
            Err(_) => {
                errors.push(ResourceError {
                    resource: "credentials".into(),
                    message: "Could not load credentials resources; keeping last valid state"
                        .into(),
                });
            }
        }

        *self.errors.write().unwrap_or_else(|e| e.into_inner()) = errors;
        self.publish();
        Ok(())
    }

    pub fn publish(&self) {
        let next = self.revision.fetch_add(1, Ordering::AcqRel) + 1;
        let _ = self.changes.send(next);
    }

    pub async fn save_settings(&self, value: Value) -> Result<()> {
        if !value.is_object() {
            return Err(Error::Invalid("settings must be a JSON object".into()));
        }
        tokio::fs::create_dir_all(&self.directory).await?;
        let path = self.directory.join("settings.json");
        let dir = self.directory.clone();
        let bytes = serde_json::to_vec_pretty(&value)?;
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
            tmp.write_all(&bytes)?;
            tmp.write_all(b"\n")?;
            tmp.persist(&path).map_err(|e| e.error)?;
            Ok(())
        })
        .await
        .map_err(|e| Error::Invalid(e.to_string()))??;
        self.reload().await
    }

    pub fn snapshot(&self) -> ResourceSnapshot {
        let settings = self.settings();
        let credentials = self
            .credentials_cache
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let errors = self
            .errors
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let providers = self
            .models
            .get_providers()
            .into_iter()
            .map(|p| {
                let mut auth_methods = Vec::new();
                let mut login_methods = Vec::new();
                if let Some(api_key) = &p.auth.api_key {
                    auth_methods.push("api_key".to_owned());
                    login_methods.push(LoginMethodInfo {
                        kind: "api_key".to_owned(),
                        label: api_key.name().to_owned(),
                    });
                }
                if let Some(oauth) = &p.auth.oauth {
                    auth_methods.push("oauth".to_owned());
                    login_methods.push(LoginMethodInfo {
                        kind: "oauth".to_owned(),
                        label: oauth.name().to_owned(),
                    });
                }
                ProviderSummary {
                    id: p.id,
                    name: p.name,
                    auth_methods,
                    login_methods,
                }
            })
            .collect();
        let models = self
            .models
            .get_models(None)
            .into_iter()
            .map(|m| {
                let thinking_levels = m
                    .supported_thinking_levels()
                    .map(|l| l.as_str().to_owned())
                    .collect();
                ModelInfo {
                    provider: m.base.provider,
                    model_id: m.base.id,
                    name: m.base.name,
                    thinking_levels,
                    context_window: m.context_window,
                }
            })
            .collect();

        ResourceSnapshot {
            version: self.revision.load(Ordering::Acquire),
            settings,
            credentials,
            providers,
            models,
            errors,
        }
    }

    pub fn summary(&self) -> ModelSummary {
        let snapshot = self.snapshot();
        let mut signed_in: HashSet<&str> = snapshot
            .credentials
            .iter()
            .map(|c| c.provider_id.as_str())
            .collect();
        if self.fake {
            signed_in.insert("faux");
        }
        let models: Vec<ModelInfo> = snapshot
            .models
            .iter()
            .filter(|m| signed_in.contains(m.provider.as_str()))
            .cloned()
            .collect();

        let configured = snapshot
            .settings
            .get("defaultModel")
            .and_then(|v| {
                let provider = v.get("provider")?.as_str()?.to_owned();
                let model_id = v.get("modelId")?.as_str()?.to_owned();
                Some((provider, model_id))
            })
            .or_else(env_default_model);

        let wanted = snapshot
            .settings
            .get("thinkingLevel")
            .and_then(Value::as_str)
            .unwrap_or("high");

        let clamp_level = |levels: Option<&[String]>| -> String {
            match levels {
                Some(levels) if !levels.is_empty() => {
                    if levels.iter().any(|l| l == wanted) {
                        wanted.to_owned()
                    } else {
                        levels.last().cloned().unwrap_or_else(|| wanted.to_owned())
                    }
                }
                _ => wanted.to_owned(),
            }
        };

        let fallback = if let Some((ref prov, ref id)) = configured {
            snapshot
                .models
                .iter()
                .find(|m| m.provider == *prov && m.model_id == *id)
        } else {
            models.first()
        };

        let default_model = if let Some((provider, model_id)) = configured {
            Some(ModelChoice {
                provider,
                model_id,
                thinking_level: clamp_level(fallback.map(|m| m.thinking_levels.as_slice())),
            })
        } else {
            fallback.map(|m| ModelChoice {
                provider: m.provider.clone(),
                model_id: m.model_id.clone(),
                thinking_level: clamp_level(Some(m.thinking_levels.as_slice())),
            })
        };

        let errors = snapshot.errors.into_iter().map(|e| e.message).collect();
        ModelSummary {
            models,
            default_model,
            errors,
        }
    }
}

fn env_default_model() -> Option<(String, String)> {
    let provider = std::env::var("PI_DEFAULT_PROVIDER")
        .ok()
        .filter(|s| !s.is_empty())?;
    let model = std::env::var("PI_DEFAULT_MODEL")
        .ok()
        .filter(|s| !s.is_empty())?;
    Some((provider, model))
}

async fn read_json_object(path: &Path) -> std::result::Result<Value, String> {
    let raw = match tokio::fs::read_to_string(path).await {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(json!({})),
        Err(_) => {
            return Err(format!(
                "Could not load {} resources; keeping last valid state",
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("config")
            ));
        }
    };
    let trimmed = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    let parsed: Value = serde_json::from_str(trimmed).map_err(|_| {
        format!(
            "Could not load {} resources; keeping last valid state",
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("config")
        )
    })?;
    if !parsed.is_object() {
        return Err(format!(
            "Could not load {} resources; keeping last valid state",
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("config")
        ));
    }
    Ok(parsed)
}
