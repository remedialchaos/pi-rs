//! Coordinates the store's scoped commits with live document owners.
use super::Host;
use crate::{
    Error, Result,
    context::lock,
    contracts::Address,
    store::{HostDataCounts, HostDataExisting, HostDataOwners, PreparedHostData, StoreEngine},
};

impl Host {
    fn ownership_store(&self) -> Result<&StoreEngine> {
        self.0
            .options
            .store
            .as_ref()
            .ok_or_else(|| Error::Invalid("host has no store".into()))
    }

    pub async fn export_host_data(&self, address: &Address) -> Result<Vec<u8>> {
        self.ownership_store()?.export_host_data(address).await
    }

    pub fn prepare_host_data(
        &self,
        source: &Address,
        target: &Address,
        bytes: &[u8],
    ) -> Result<PreparedHostData> {
        self.ownership_store()?
            .prepare_host_data(source, target, bytes)
    }

    pub async fn import_host_data(
        &self,
        archive: PreparedHostData,
        existing: HostDataExisting,
    ) -> Result<HostDataCounts> {
        let store = self.ownership_store()?;
        store
            .import_owned_host_data(archive, existing, self.host_data_owners().await?)
            .await
    }

    pub async fn delete_host_data(&self, address: &Address) -> Result<HostDataCounts> {
        let store = self.ownership_store()?;
        store
            .delete_owned_host_data(address, self.host_data_owners().await?)
            .await
    }

    async fn host_data_owners(&self) -> Result<HostDataOwners> {
        let gate = self.0.host_data.clone().write_owned().await;
        let mut documents = {
            let state = lock(&self.0.state);
            if state.closed {
                return Err(Error::Closed);
            }
            state
                .objects
                .iter()
                .map(|(key, object)| (key.clone(), object.documents.clone()))
                .collect::<Vec<_>>()
        };
        documents.sort_by(|a, b| a.0.cmp(&b.0));
        let mut held = Vec::with_capacity(documents.len());
        for (_, document) in documents {
            let lease = document.lock_host_data().await?;
            held.push((document, lease));
        }
        Ok(HostDataOwners {
            documents: held,
            _gate: Some(gate),
        })
    }
}
