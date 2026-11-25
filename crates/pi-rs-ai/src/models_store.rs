use async_trait::async_trait;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::http::abortable;
use crate::model::AnyModel;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsStoreEntry {
    pub models: Vec<AnyModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}
#[async_trait]
pub trait ModelsStore: Send + Sync {
    async fn read(
        &self,
        provider: &str,
        signal: &CancellationToken,
    ) -> Result<Option<ModelsStoreEntry>>;
    async fn write(
        &self,
        provider: &str,
        entry: ModelsStoreEntry,
        signal: &CancellationToken,
    ) -> Result<()>;
    async fn delete(&self, provider: &str, signal: &CancellationToken) -> Result<()>;
}
#[derive(Default)]
pub struct InMemoryModelsStore(RwLock<IndexMap<String, ModelsStoreEntry>>);
#[async_trait]
impl ModelsStore for InMemoryModelsStore {
    async fn read(
        &self,
        provider: &str,
        signal: &CancellationToken,
    ) -> Result<Option<ModelsStoreEntry>> {
        abortable(signal, async {
            Ok(self.0.read().await.get(provider).cloned())
        })
        .await
    }
    async fn write(
        &self,
        provider: &str,
        entry: ModelsStoreEntry,
        signal: &CancellationToken,
    ) -> Result<()> {
        abortable(signal, async {
            self.0.write().await.insert(provider.into(), entry);
            Ok(())
        })
        .await
    }
    async fn delete(&self, provider: &str, signal: &CancellationToken) -> Result<()> {
        abortable(signal, async {
            self.0.write().await.shift_remove(provider);
            Ok(())
        })
        .await
    }
}
