use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
};

use pi_rs_chord::context::Context;

use crate::env::{ExecutionEnv, FileError, FileErrorCode};

use super::ToolError;

type MutationKey = (String, String);
type FileLock = Arc<tokio::sync::Mutex<()>>;
static QUEUES: LazyLock<Mutex<HashMap<Arc<MutationKey>, FileLock>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct Slot {
    key: Arc<MutationKey>,
    lock: FileLock,
}

impl Slot {
    fn new(key: MutationKey) -> Self {
        let mut queues = QUEUES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((key, lock)) = queues.get_key_value(&key) {
            return Self {
                key: Arc::clone(key),
                lock: Arc::clone(lock),
            };
        }
        let key = Arc::new(key);
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        queues.insert(Arc::clone(&key), Arc::clone(&lock));
        Self { key, lock }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut queues = QUEUES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Only this Slot and the registry still reference the mutex. Taking the registry lock means
        // no newcomer can get a separate mutex while a waiter still holds this one.
        if Arc::strong_count(&self.lock) == 2 {
            queues.remove(self.key.as_ref());
        }
    }
}

async fn canonical(
    env: &dyn ExecutionEnv,
    mut path: String,
    context: &Context,
) -> Result<String, FileError> {
    let mut missing = Vec::new();
    let mut base = loop {
        match env.canonical_path(&path, context).await {
            Ok(canonical) => break canonical,
            Err(error) if error.code == FileErrorCode::NotSupported => break path,
            Err(error) if error.code == FileErrorCode::NotFound => {
                let parent = env.join_path(&[&path, ".."], context).await?;
                let Some(suffix) = path
                    .strip_prefix(&parent)
                    .filter(|suffix| !suffix.is_empty())
                else {
                    break path;
                };
                let name = if parent.ends_with('/')
                    || (parent.ends_with('\\') && !suffix.starts_with('/'))
                {
                    suffix
                } else if let Some(name) = suffix.strip_prefix(['/', '\\']) {
                    name
                } else {
                    break path;
                };
                missing.push(name.to_owned());
                path = parent;
            }
            Err(error) => return Err(error),
        }
    };
    for name in missing.iter().rev() {
        base = env.join_path(&[&base, name], context).await?;
    }
    Ok(base)
}

/// Serializes mutations per physical file system and canonical path, in key resolution order.
/// Abort via `context`; a running mutation holds the slot until its future finishes.
pub async fn with_file_mutation_queue<T>(
    env: &dyn ExecutionEnv,
    path: &str,
    operation: impl AsyncFnOnce() -> Result<T, ToolError>,
    context: &Context,
) -> Result<T, ToolError> {
    let absolute = env.absolute_path(path, context).await?;
    let key = (
        env.id().to_owned(),
        canonical(env, absolute, context).await?,
    );
    let slot = Slot::new(key);
    let _guard = slot.lock.lock().await;
    operation().await
}
