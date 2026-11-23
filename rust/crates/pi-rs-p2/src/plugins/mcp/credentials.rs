//! Private, atomically replaced OAuth state for the owner's MCP servers.

use std::{
    collections::HashMap,
    future::Future,
    io::{self, Write},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, Weak},
};

use pi_rs_mcp::{
    mcp_namespace,
    oauth::{
        LockFn, McpOAuthServerStore, McpOAuthState, McpOAuthStateStore, McpStoreError,
        provider::{LoadFuture, SaveFuture},
    },
};
use serde_json::{Map, Value};
use tokio::sync::Mutex as AsyncMutex;
use url::Url;

type RefreshLocks = Arc<Mutex<HashMap<String, Weak<AsyncMutex<()>>>>>;

#[derive(Clone)]
pub struct Credentials {
    file: Arc<PathBuf>,
    writes: Arc<AsyncMutex<()>>,
    refresh: RefreshLocks,
}

impl Credentials {
    pub fn new(data: &Path) -> Self {
        Self {
            file: Arc::new(data.join("mcp-auth.json")),
            writes: Arc::default(),
            refresh: Arc::default(),
        }
    }

    pub fn server(
        &self,
        name: &str,
        url: &str,
    ) -> Result<Arc<dyn McpOAuthServerStore>, McpStoreError> {
        let key = format!(
            "{}|{}",
            mcp_namespace(name),
            Url::parse(url).map_err(store_error)?
        );
        let mut refresh = self
            .refresh
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        refresh.retain(|_, lock| lock.strong_count() != 0);
        let lock = refresh
            .get(&key)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let lock = Arc::new(AsyncMutex::new(()));
                refresh.insert(key.clone(), Arc::downgrade(&lock));
                lock
            });
        Ok(Arc::new(ServerStore {
            credentials: self.clone(),
            key,
            refresh: lock,
        }))
    }

    pub async fn remove(&self, name: &str, url: &str) -> Result<(), McpStoreError> {
        let key = format!(
            "{}|{}",
            mcp_namespace(name),
            Url::parse(url).map_err(store_error)?
        );
        let write = self.writes.clone().lock_owned().await;
        let mut states = self.read().await?;
        if states.shift_remove(&key).is_some() {
            self.write(states, write).await?;
        }
        Ok(())
    }

    async fn read(&self) -> Result<Map<String, Value>, McpStoreError> {
        let text = match tokio::fs::read_to_string(&*self.file).await {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Map::new()),
            Err(error) => return Err(store_error(error)),
        };
        let text = text.trim_start_matches('\u{feff}').trim();
        if text.is_empty() {
            return Ok(Map::new());
        }
        let value = serde_json::from_str::<Value>(text).map_err(store_error)?;
        Ok(if let Value::Object(states) = value {
            states
        } else {
            Map::new()
        })
    }

    async fn write(
        &self,
        states: Map<String, Value>,
        write: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<(), McpStoreError> {
        let path = self.file.clone();
        tokio::task::spawn_blocking(move || {
            let _write = write;
            let directory = path
                .parent()
                .ok_or_else(|| store_error("Missing credential directory"))?;
            let mut file = tempfile::NamedTempFile::new_in(directory).map_err(store_error)?;
            serde_json::to_writer_pretty(&mut file, &states).map_err(store_error)?;
            file.write_all(b"\n").map_err(store_error)?;
            file.as_file().sync_all().map_err(store_error)?;
            file.persist(&*path).map_err(store_error)?;
            Ok(())
        })
        .await
        .map_err(store_error)?
    }
}

struct ServerStore {
    credentials: Credentials,
    key: String,
    refresh: Arc<AsyncMutex<()>>,
}

impl McpOAuthStateStore for ServerStore {
    fn load(&self) -> LoadFuture {
        let (credentials, key) = (self.credentials.clone(), self.key.clone());
        Box::pin(async move {
            let _write = credentials.writes.lock().await;
            credentials
                .read()
                .await?
                .remove(&key)
                .map(serde_json::from_value)
                .transpose()
                .map_err(store_error)
        })
    }

    fn save(&self, state: McpOAuthState) -> SaveFuture {
        let (credentials, key) = (self.credentials.clone(), self.key.clone());
        Box::pin(async move {
            let write = credentials.writes.clone().lock_owned().await;
            let mut states = credentials.read().await?;
            states.insert(key, serde_json::to_value(state).map_err(store_error)?);
            credentials.write(states, write).await
        })
    }
}

impl McpOAuthServerStore for ServerStore {
    fn with_refresh_lock(
        &self,
        run: LockFn,
    ) -> Pin<Box<dyn Future<Output = Result<(), McpStoreError>> + Send>> {
        let refresh = self.refresh.clone();
        Box::pin(async move {
            let _refresh = refresh.lock().await;
            run().await
        })
    }
}

fn store_error(error: impl std::fmt::Display) -> McpStoreError {
    McpStoreError::Other(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn credential_reopen_and_sign_out_are_scoped_to_server_name_and_url()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let url = "https://example.test/mcp";
        let state = McpOAuthState {
            server_url: url.into(),
            code_verifier: Some("fixture-verifier".into()),
            ..Default::default()
        };
        let credentials = Credentials::new(directory.path());
        credentials.server("one", url)?.save(state.clone()).await?;
        drop(credentials);
        let reopened = Credentials::new(directory.path());
        assert_eq!(
            reopened.server("one", url)?.load().await?,
            Some(state.clone())
        );
        assert_eq!(reopened.server("two", url)?.load().await?, None);
        assert_eq!(
            reopened
                .server("one", "https://elsewhere.test/mcp")?
                .load()
                .await?,
            None
        );
        reopened.server("two", url)?.save(state.clone()).await?;
        reopened.remove("one", url).await?;
        assert_eq!(reopened.server("one", url)?.load().await?, None);
        assert_eq!(reopened.server("two", url)?.load().await?, Some(state));
        Ok(())
    }
}
