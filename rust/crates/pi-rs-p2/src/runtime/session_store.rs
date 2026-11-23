use super::host_dirs::{open_host_store, session_directory};
use pi_rs_chord::store::StoreEngine;
use pi_rs_durable::{
    Storage,
    errors::BoxError,
    storage::{StorageResult, store::StoreStorage},
};
use std::{path::Path, sync::Arc};

pub const DURABLE_OWNER: &str = "p2.durable";

pub fn durable_storage(store: &StoreEngine) -> StorageResult<StoreStorage> {
    StoreStorage::from_store(store.clone(), DURABLE_OWNER)
}

pub struct SessionStorage {
    pub store: StoreEngine,
    pub storage: Arc<StoreStorage>,
}

impl SessionStorage {
    pub async fn close(self) -> Result<(), BoxError> {
        let released = self.storage.close();
        self.store.database().close().await?;
        Ok(released?)
    }
}

pub async fn open_session_storage(data_dir: &Path, id: &str) -> Result<SessionStorage, BoxError> {
    let store = open_host_store(&session_directory(data_dir, id)?).await?;
    match durable_storage(&store) {
        Ok(storage) => Ok(SessionStorage {
            store,
            storage: Arc::new(storage),
        }),
        Err(error) => {
            store.database().close().await?;
            Err(error.into())
        }
    }
}
