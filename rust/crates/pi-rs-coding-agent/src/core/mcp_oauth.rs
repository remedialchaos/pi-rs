use futures_util::future::BoxFuture;
use pi_rs_mcp::oauth::*;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::core::auth_storage::{ensure_file_exists, ensure_parent_dir};
use crate::lockfile::{LockOptions, lock_sync};

#[derive(Clone)]
pub(crate) struct Store {
    path: PathBuf,
    key: String,
    url: String,
}

impl Store {
    pub fn new(dir: &Path, name: &str, url: &str) -> Result<Self, String> {
        let url = url::Url::parse(url).map_err(|e| e.to_string())?.to_string();
        Ok(Self {
            path: dir.join("mcp-auth.json"),
            key: format!("{}|{url}", pi_rs_mcp::mcp_namespace(name)),
            url,
        })
    }

    fn update<T>(
        &self,
        run: impl FnOnce(&mut Map<String, Value>) -> Result<(T, bool), String>,
    ) -> Result<T, String> {
        ensure_parent_dir(&self.path)?;
        ensure_file_exists(&self.path)?;
        let _guard = lock_sync(
            &self.path,
            &LockOptions {
                retries: 10,
                ..Default::default()
            },
        )?;
        let data = std::fs::read_to_string(&self.path).map_err(|e| e.to_string())?;
        let mut entries = serde_json::from_str(&data).map_err(|e| e.to_string())?;
        let (result, changed) = run(&mut entries)?;
        if changed {
            let text = serde_json::to_string_pretty(&entries).map_err(|e| e.to_string())?;
            super::auth_storage::write_file(&self.path, &format!("{text}\n"))?;
        }
        Ok(result)
    }

    pub fn remove(&self) -> Result<bool, String> {
        self.update(|entries| {
            let removed = entries
                .shift_remove(&self.key)
                .or_else(|| entries.shift_remove(&self.url))
                .is_some();
            Ok((removed, removed))
        })
    }
}

impl McpOAuthStateStore for Store {
    fn load(&self) -> provider::LoadFuture {
        let store = self.clone();
        Box::pin(async move {
            store
                .update(|entries| {
                    let migrated =
                        !entries.contains_key(&store.key) && entries.contains_key(&store.url);
                    if migrated && let Some(old) = entries.shift_remove(&store.url) {
                        entries.insert(store.key.clone(), old);
                    }
                    let state = entries
                        .get(&store.key)
                        .cloned()
                        .map(serde_json::from_value)
                        .transpose()
                        .map_err(|e| e.to_string())?;
                    Ok((state, migrated))
                })
                .map_err(McpStoreError::Other)
        })
    }
    fn save(&self, state: McpOAuthState) -> provider::SaveFuture {
        let store = self.clone();
        Box::pin(async move {
            store
                .update(|entries| {
                    entries.insert(
                        store.key.clone(),
                        serde_json::to_value(state).map_err(|e| e.to_string())?,
                    );
                    Ok(((), true))
                })
                .map_err(McpStoreError::Other)
        })
    }
}

impl McpOAuthServerStore for Store {
    fn with_refresh_lock(&self, run: LockFn) -> BoxFuture<'static, Result<(), McpStoreError>> {
        let digest = format!("{:x}", Sha256::digest(self.key.as_bytes()));
        let path = self
            .path
            .with_file_name(format!("mcp-auth-refresh-{}", &digest[..16]));
        Box::pin(async move {
            ensure_parent_dir(&path).map_err(McpStoreError::Other)?;
            let _guard = tokio::task::spawn_blocking(move || {
                lock_sync(
                    &path,
                    &LockOptions {
                        retries: 10,
                        realpath: false,
                        stale_ms: 120_000,
                        ..Default::default()
                    },
                )
            })
            .await
            .map_err(|e| McpStoreError::Other(e.to_string()))?
            .map_err(|e| {
                if e.contains("ELOCKED") {
                    McpStoreError::RefreshLockBusy
                } else {
                    McpStoreError::Other(e)
                }
            })?;
            run().await
        })
    }
}

pub(crate) fn settings(name: &str, data: &Value) -> Result<McpOAuthSettings, String> {
    let string = |key: &str| data[key].as_str().map(str::to_owned);
    Ok(McpOAuthSettings {
        client_id: string("clientId"),
        client_secret: data["clientSecret"].as_str().and_then(|s| {
            crate::core::resolve_config_value::resolve_config_value_str(s, name, "clientSecret")
        }),
        client_name: string("clientName"),
        callback_port: data["callbackPort"]
            .as_u64()
            .and_then(|p| u16::try_from(p).ok()),
        callback_url: string("callbackUrl"),
        scope: string("scope"),
        client_registration: client_registration_from_value(data.get("clientRegistration")),
        auth_server_metadata_url: data["authServerMetadataUrl"]
            .as_str()
            .map(url::Url::parse)
            .transpose()
            .map_err(|e| e.to_string())?,
    })
}

pub(crate) async fn login(
    dir: &Path,
    name: &str,
    url: &str,
    data: &Value,
    timeout: u64,
    challenge: Option<OAuthChallenge>,
) -> Result<(), String> {
    let settings = settings(name, data)?;
    let store: Arc<dyn McpOAuthServerStore> = Arc::new(Store::new(dir, name, url)?);
    let listener = listen_for_loopback_sign_in(LoopbackSignInOptions {
        server_url: url,
        store: store.clone(),
        settings: &settings,
        client_metadata_documents: None,
        render_page: None,
    })
    .await
    .map_err(|e| e.to_string())?;
    let options = McpSignInOptions {
        server_url: url.to_owned(),
        store,
        settings,
        client_name: "pi".to_owned(),
        client_metadata_documents: None,
        redirect_url: listener.redirect_url,
        challenge,
        signal: None,
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(timeout), async {
        if let McpSignInStart::Redirect(auth) = start_mcp_sign_in(options.clone()).await? {
            eprintln!("Open this URL to sign in: {}", auth.authorization_url);
            let _ = crate::utils::open_browser::try_open_browser(&auth.authorization_url);
            let response = wait_for_loopback_sign_in(WaitForLoopbackOptions {
                callback: listener.callback.clone(),
                authorization: auth.clone(),
                prompt_for_redirect_url: None,
                signal: None,
            })
            .await?;
            complete_mcp_sign_in(options, auth, response).await?;
        }
        Ok::<_, OAuthError>(())
    })
    .await;
    listener.callback.close().await;
    result
        .map_err(|_| "Sign-in timed out".to_owned())?
        .map_err(|e| e.to_string())
}
