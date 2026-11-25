use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock};
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};
use crate::http::abortable;
use crate::request::{ProviderEnv, ProviderHeaders};
use crate::types::{JsonObject, unix_millis};

pub const OAUTH_MIN_VALIDITY: Duration = Duration::from_secs(300);
pub const OAUTH_REFRESH_TIMEOUT: Duration = Duration::from_secs(15);
#[derive(Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelAuth {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<ProviderHeaders>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}
#[derive(Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApiKeyCredential {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub env: ProviderEnv,
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct OAuthCredential {
    pub refresh: String,
    pub access: String,
    pub expires: u64,
    #[serde(flatten)]
    pub details: JsonObject,
}
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Credential {
    ApiKey(ApiKeyCredential),
    #[serde(rename = "oauth")]
    OAuth(OAuthCredential),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthType {
    ApiKey,
    #[serde(rename = "oauth")]
    OAuth,
}
impl Credential {
    pub fn auth_type(&self) -> AuthType {
        match self {
            Self::ApiKey(_) => AuthType::ApiKey,
            Self::OAuth(_) => AuthType::OAuth,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialInfo {
    pub provider_id: String,
    #[serde(rename = "type")]
    pub auth_type: AuthType,
}
#[derive(Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuthResult {
    pub auth: ModelAuth,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub env: ProviderEnv,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuthCheck {
    pub source: Option<String>,
    #[serde(rename = "type")]
    pub auth_type: AuthType,
}

#[async_trait]
pub trait AuthContext: Send + Sync {
    async fn env(&self, name: &str) -> Option<String>;
    async fn file_exists(&self, path: &str) -> bool;
}
#[derive(Default)]
pub struct SystemAuthContext;
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}
pub fn home_path(path: &str) -> PathBuf {
    path.strip_prefix("~/")
        .and_then(|inner| home().map(|home| home.join(inner)))
        .unwrap_or_else(|| PathBuf::from(path))
}
#[async_trait]
impl AuthContext for SystemAuthContext {
    async fn env(&self, name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.is_empty())
    }
    async fn file_exists(&self, path: &str) -> bool {
        tokio::fs::try_exists(home_path(path))
            .await
            .unwrap_or(false)
    }
}
struct Overlay<'a>(&'a dyn AuthContext, &'a ProviderEnv);
#[async_trait]
impl AuthContext for Overlay<'_> {
    async fn env(&self, name: &str) -> Option<String> {
        if let Some(v) = self.1.get(name).filter(|v| !v.is_empty()) {
            Some(v.clone())
        } else {
            self.0.env(name).await
        }
    }
    async fn file_exists(&self, path: &str) -> bool {
        self.0.file_exists(path).await
    }
}

pub type Mutation =
    Box<dyn FnOnce(Option<Credential>) -> BoxFuture<'static, Result<Option<Credential>>> + Send>;
#[async_trait]
pub trait CredentialStore: Send + Sync {
    async fn read(&self, provider: &str, signal: &CancellationToken) -> Result<Option<Credential>>;
    async fn list(&self, signal: &CancellationToken) -> Result<Vec<CredentialInfo>>;
    /// `None` from the callback preserves the entry. Cancellation gates lock acquisition, not the mutation after the callback starts.
    async fn modify(
        &self,
        provider: &str,
        update: Mutation,
        signal: &CancellationToken,
    ) -> Result<Option<Credential>>;
    async fn delete(&self, provider: &str, signal: &CancellationToken) -> Result<()>;
}
#[derive(Default)]
pub struct InMemoryCredentialStore {
    entries: RwLock<IndexMap<String, Credential>>,
    locks: Mutex<IndexMap<String, std::sync::Weak<Mutex<()>>>>,
}
impl InMemoryCredentialStore {
    async fn lock(
        &self,
        provider: &str,
        signal: &CancellationToken,
    ) -> Result<OwnedMutexGuard<()>> {
        let mut slots = abortable(signal, async { Ok(self.locks.lock().await) }).await?;
        let lock = if let Some(existing) = slots.get(provider).and_then(std::sync::Weak::upgrade) {
            existing
        } else {
            slots.retain(|_, v| v.strong_count() != 0);
            let lock = Arc::new(Mutex::new(()));
            slots.insert(provider.into(), Arc::downgrade(&lock));
            lock
        };
        drop(slots);
        abortable(signal, async { Ok(lock.lock_owned().await) }).await
    }
}
#[async_trait]
impl CredentialStore for InMemoryCredentialStore {
    async fn read(&self, provider: &str, signal: &CancellationToken) -> Result<Option<Credential>> {
        abortable(signal, async {
            Ok(self.entries.read().await.get(provider).cloned())
        })
        .await
    }
    async fn list(&self, signal: &CancellationToken) -> Result<Vec<CredentialInfo>> {
        abortable(signal, async {
            Ok(self
                .entries
                .read()
                .await
                .iter()
                .map(|(provider, c)| CredentialInfo {
                    provider_id: provider.clone(),
                    auth_type: c.auth_type(),
                })
                .collect())
        })
        .await
    }
    async fn modify(
        &self,
        provider: &str,
        update: Mutation,
        signal: &CancellationToken,
    ) -> Result<Option<Credential>> {
        let _guard = self.lock(provider, signal).await?;
        let current = self.read(provider, signal).await?;
        if let Some(next) = update(current.clone()).await? {
            self.entries
                .write()
                .await
                .insert(provider.into(), next.clone());
            Ok(Some(next))
        } else {
            Ok(current)
        }
    }
    async fn delete(&self, provider: &str, signal: &CancellationToken) -> Result<()> {
        let _guard = self.lock(provider, signal).await?;
        self.entries.write().await.shift_remove(provider);
        Ok(())
    }
}

#[derive(Clone)]
pub struct FileCredentialStore {
    path: PathBuf,
}
impl FileCredentialStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    fn auxiliary(&self, suffix: &str) -> PathBuf {
        let mut path = self.path.as_os_str().to_owned();
        path.push(suffix);
        PathBuf::from(path)
    }
    async fn lock(&self, suffix: &str, signal: &CancellationToken) -> Result<std::fs::File> {
        if let Some(dir) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(dir).await?;
        }
        let path = self.auxiliary(suffix);
        let mut builder = tokio::fs::OpenOptions::new();
        builder.create(true).read(true).write(true);
        #[cfg(unix)]
        builder.mode(0o600);
        let file = abortable(signal, async {
            Ok(builder.open(path).await?.into_std().await)
        })
        .await?;
        loop {
            if signal.is_cancelled() {
                return Err(Error::Aborted);
            }
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(std::fs::TryLockError::WouldBlock) => {
                    abortable(signal, async {
                        tokio::time::sleep(Duration::from_millis(15)).await;
                        Ok(())
                    })
                    .await?
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
            }
        }
    }
    async fn entries(&self) -> Result<IndexMap<String, Credential>> {
        match tokio::fs::read(&self.path).await {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(IndexMap::new()),
            Err(e) => Err(e.into()),
        }
    }
    async fn write(&self, entries: &IndexMap<String, Credential>) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let temp = self.auxiliary(&format!(".tmp-{}", uuid::Uuid::new_v4()));
        let mut options = tokio::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temp).await?;
        let result = async {
            let bytes = serde_json::to_vec_pretty(entries)?;
            file.write_all(&bytes).await?;
            file.write_all(b"\n").await?;
            file.sync_all().await?;
            drop(file);
            tokio::fs::rename(&temp, &self.path).await?;
            Ok(())
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&temp).await;
        }
        result
    }
    async fn provider_lock(
        &self,
        provider: &str,
        signal: &CancellationToken,
    ) -> Result<std::fs::File> {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        use sha2::{Digest, Sha256};
        let suffix = format!(
            ".lock.{}",
            URL_SAFE_NO_PAD.encode(Sha256::digest(provider.as_bytes()))
        );
        self.lock(&suffix, signal).await
    }
}
#[async_trait]
impl CredentialStore for FileCredentialStore {
    async fn read(&self, provider: &str, signal: &CancellationToken) -> Result<Option<Credential>> {
        abortable(signal, async {
            Ok(self.entries().await?.shift_remove(provider))
        })
        .await
    }
    async fn list(&self, signal: &CancellationToken) -> Result<Vec<CredentialInfo>> {
        abortable(signal, async {
            Ok(self
                .entries()
                .await?
                .into_iter()
                .map(|(provider_id, c)| CredentialInfo {
                    provider_id,
                    auth_type: c.auth_type(),
                })
                .collect())
        })
        .await
    }
    async fn modify(
        &self,
        provider: &str,
        update: Mutation,
        signal: &CancellationToken,
    ) -> Result<Option<Credential>> {
        let _provider = self.provider_lock(provider, signal).await?;
        let current = self.read(provider, signal).await?;
        if let Some(next) = update(current.clone()).await? {
            let _file = self.lock(".lock", &CancellationToken::new()).await?;
            let mut entries = self.entries().await?;
            entries.insert(provider.into(), next.clone());
            self.write(&entries).await?;
            Ok(Some(next))
        } else {
            Ok(current)
        }
    }
    async fn delete(&self, provider: &str, signal: &CancellationToken) -> Result<()> {
        let _provider = self.provider_lock(provider, signal).await?;
        let _file = self.lock(".lock", signal).await?;
        let mut entries = self.entries().await?;
        if entries.shift_remove(provider).is_some() {
            self.write(&entries).await?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectOption {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PromptKind {
    Text,
    Secret,
    Select(Vec<SelectOption>),
    ManualCode,
}
#[derive(Clone, Debug)]
pub struct AuthPrompt {
    pub kind: PromptKind,
    pub message: String,
    pub placeholder: Option<String>,
    pub signal: CancellationToken,
}
#[derive(Clone, Debug)]
pub enum AuthEvent {
    Info {
        message: String,
        links: Vec<(String, Option<String>)>,
    },
    AuthUrl {
        url: String,
        instructions: Option<String>,
    },
    DeviceCode {
        user_code: String,
        verification_uri: String,
        interval_seconds: u64,
        expires_in_seconds: Option<u64>,
        open_browser_on_confirm: bool,
        copy_code_on_confirm: bool,
    },
    Progress(String),
}
#[async_trait]
pub trait AuthInteraction: Send + Sync {
    async fn prompt(&self, prompt: AuthPrompt) -> Result<String>;
    fn notify(&self, event: AuthEvent);
}
#[derive(Clone, Default)]
pub struct LoginOptions {
    pub get_device_id: Option<Arc<dyn Fn() -> String + Send + Sync>>,
    pub agent_name: Option<String>,
    pub signal: CancellationToken,
}
#[async_trait]
pub trait OAuthAuth: Send + Sync {
    fn name(&self) -> &str;
    fn is_subscription(&self) -> bool {
        false
    }
    fn login_label(&self) -> Option<&str> {
        None
    }
    async fn login(
        &self,
        interaction: &dyn AuthInteraction,
        opts: &LoginOptions,
    ) -> Result<OAuthCredential>;
    async fn refresh(
        &self,
        credential: OAuthCredential,
        signal: CancellationToken,
    ) -> Result<OAuthCredential>;
    async fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth> {
        Ok(ModelAuth {
            api_key: Some(credential.access.clone()),
            ..ModelAuth::default()
        })
    }
}
#[async_trait]
pub trait ApiKeyAuth: Send + Sync {
    fn name(&self) -> &str;
    fn interactive(&self) -> bool {
        false
    }
    async fn login(
        &self,
        _interaction: &dyn AuthInteraction,
        _opts: &LoginOptions,
    ) -> Result<ApiKeyCredential> {
        Err(Error::Auth(format!("{} is ambient-only", self.name())))
    }
    async fn check(
        &self,
        ctx: &dyn AuthContext,
        key: Option<&ApiKeyCredential>,
        signal: &CancellationToken,
    ) -> Result<Option<AuthCheck>> {
        Ok(self.resolve(ctx, key, signal).await?.map(|r| AuthCheck {
            source: r.source,
            auth_type: AuthType::ApiKey,
        }))
    }
    async fn resolve(
        &self,
        ctx: &dyn AuthContext,
        key: Option<&ApiKeyCredential>,
        signal: &CancellationToken,
    ) -> Result<Option<AuthResult>>;
}
#[derive(Clone, Default)]
pub struct ProviderAuth {
    pub api_key: Option<Arc<dyn ApiKeyAuth>>,
    pub oauth: Option<Arc<dyn OAuthAuth>>,
}
#[derive(Clone, Default)]
pub struct AuthOverrides {
    pub api_key: Option<String>,
    pub env: ProviderEnv,
    pub min_oauth_validity: Option<Duration>,
    pub signal: CancellationToken,
}
fn locked_error(label: &str, provider: &str, e: Error) -> Error {
    if matches!(e, Error::Aborted) {
        e
    } else {
        Error::Auth(format!(
            "Credential store {label} failed for {provider}: {e}"
        ))
    }
}

pub async fn refresh_stored_oauth(
    store: Arc<dyn CredentialStore>,
    provider: &str,
    oauth: Arc<dyn OAuthAuth>,
    needs_refresh: impl Fn(&OAuthCredential) -> bool + Send + 'static,
    signal: &CancellationToken,
) -> Result<Option<OAuthCredential>> {
    let provider = provider.to_owned();
    let wait = signal.clone();
    let task = tokio::spawn(async move {
        let cancel = wait.clone();
        let name = provider.clone();
        let updated = store
            .modify(
                &provider,
                Box::new(move |current| {
                    Box::pin(async move {
                        if cancel.is_cancelled() {
                            return Err(Error::Aborted);
                        }
                        let Some(Credential::OAuth(current)) = current else {
                            return Ok(None);
                        };
                        if !needs_refresh(&current) {
                            return Ok(None);
                        }
                        let refresh_signal = CancellationToken::new();
                        let refresh = oauth.refresh(current, refresh_signal.clone());
                        let next = match tokio::time::timeout(OAUTH_REFRESH_TIMEOUT, refresh).await
                        {
                            Ok(Ok(next)) => next,
                            Ok(Err(e)) => {
                                return Err(Error::OAuth(format!(
                                    "OAuth refresh failed for {name}: {e}"
                                )));
                            }
                            Err(_) => {
                                refresh_signal.cancel();
                                return Err(Error::OAuth(format!(
                                    "OAuth refresh failed for {name}: timed out after 15s"
                                )));
                            }
                        };
                        Ok(Some(Credential::OAuth(next)))
                    })
                }),
                &wait,
            )
            .await
            .map_err(|e| {
                if matches!(e, Error::OAuth(_)) {
                    e
                } else {
                    locked_error("modify", &provider, e)
                }
            })?;
        Ok(match updated {
            Some(Credential::OAuth(o)) => Some(o),
            _ => None,
        })
    });
    abortable(signal, async {
        task.await
            .map_err(|e| Error::OAuth(format!("OAuth refresh task failed: {e}")))?
    })
    .await
}
pub async fn resolve_provider_auth(
    provider: &str,
    auth: &ProviderAuth,
    store: Arc<dyn CredentialStore>,
    ctx: &dyn AuthContext,
    options: &AuthOverrides,
) -> Result<Option<AuthResult>> {
    let signal = &options.signal;
    abortable(signal, async {
        let context = Overlay(ctx, &options.env);
        if let Some(key) = &options.api_key
            && let Some(api) = &auth.api_key
        {
            return api
                .resolve(
                    &context,
                    Some(&ApiKeyCredential {
                        key: Some(key.clone()),
                        env: options.env.clone(),
                    }),
                    signal,
                )
                .await
                .map_err(|e| {
                    Error::Auth(format!("API key auth failed for provider {provider}: {e}"))
                });
        }
        let saved = store
            .read(provider, signal)
            .await
            .map_err(|e| locked_error("read", provider, e))?;
        match saved {
            Some(Credential::OAuth(mut value)) => {
                let Some(oauth) = &auth.oauth else {
                    return Ok(None);
                };
                let minimum = u64::try_from(
                    options
                        .min_oauth_validity
                        .unwrap_or_default()
                        .max(OAUTH_MIN_VALIDITY)
                        .as_millis(),
                )
                .unwrap_or(u64::MAX);
                let needs =
                    move |c: &OAuthCredential| unix_millis().saturating_add(minimum) >= c.expires;
                if needs(&value) {
                    let Some(updated) =
                        refresh_stored_oauth(store, provider, oauth.clone(), needs, signal).await?
                    else {
                        return Ok(None);
                    };
                    value = updated;
                    if options.min_oauth_validity.is_some() && needs(&value) {
                        return Err(Error::OAuth(format!(
                            "OAuth refresh returned a token that expires too soon for {provider}"
                        )));
                    }
                }
                let auth = oauth.to_auth(&value).await.map_err(|e| {
                    Error::OAuth(format!("OAuth auth derivation failed for {provider}: {e}"))
                })?;
                Ok(Some(AuthResult {
                    auth,
                    source: Some("OAuth".into()),
                    env: ProviderEnv::new(),
                }))
            }
            saved => {
                let Some(api) = &auth.api_key else {
                    return Ok(None);
                };
                let saved = saved.and_then(|s| match s {
                    Credential::ApiKey(mut selected) => {
                        selected.env.extend(options.env.clone());
                        Some(selected)
                    }
                    _ => None,
                });
                api.resolve(&context, saved.as_ref(), signal)
                    .await
                    .map_err(|e| {
                        if matches!(e, Error::Aborted) {
                            e
                        } else {
                            Error::Auth(format!("API key auth failed for provider {provider}: {e}"))
                        }
                    })
            }
        }
    })
    .await
}
