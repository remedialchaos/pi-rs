//! Observed `envs.json` store: keeps last valid configs on parse failure.

use std::{
    path::PathBuf,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use pi_rs_chord::Result;
use tokio::sync::watch;

use crate::plugins::envs::config::{EnvsConfig, load_envs};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnvSnapshot {
    pub configs: EnvsConfig,
    pub error: Option<String>,
}

pub struct EnvStore {
    directory: PathBuf,
    snapshot: RwLock<EnvSnapshot>,
    revision: AtomicU64,
    changes: watch::Sender<u64>,
}

impl EnvStore {
    pub async fn open(directory: PathBuf) -> Result<Arc<Self>> {
        let (changes, _) = watch::channel(0u64);
        let store = Arc::new(Self {
            directory,
            snapshot: RwLock::new(EnvSnapshot::default()),
            revision: AtomicU64::new(0),
            changes,
        });
        store.reload().await;
        Ok(store)
    }

    pub fn snapshot(&self) -> EnvSnapshot {
        self.snapshot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    pub async fn reload(&self) {
        let next = match load_envs(&self.directory).await {
            Ok(configs) => EnvSnapshot {
                configs,
                error: None,
            },
            Err(err) => {
                let mut current = self.snapshot();
                current.error = Some(err.to_string());
                current
            }
        };
        *self.snapshot.write().unwrap_or_else(|e| e.into_inner()) = next;
        let rev = self.revision.fetch_add(1, Ordering::AcqRel) + 1;
        let _ = self.changes.send(rev);
    }
}
