//! Per-file async serialization queue for `edit` and `write` tools.
//!
//! Port of `packages/coding-agent/src/core/tools/file-mutation-queue.ts`.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use super::path_utils::resolve_to_cwd;

static SHARED_QUEUE: LazyLock<Arc<FileMutationQueue>> =
    LazyLock::new(|| Arc::new(FileMutationQueue::new()));

pub fn shared_file_mutation_queue() -> Arc<FileMutationQueue> {
    SHARED_QUEUE.clone()
}

/// Serializes concurrent mutations targeting the same canonical file path while
/// allowing operations on distinct files to execute concurrently.
#[derive(Debug, Default)]
pub struct FileMutationQueue {
    queues: Mutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>,
}

impl FileMutationQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn canonical_key(path: &Path) -> Result<PathBuf, std::io::Error> {
        let resolved = std::path::absolute(path)?;
        match tokio::fs::canonicalize(&resolved).await {
            Ok(canonical) => Ok(canonical),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(resolved)
            }
            Err(error) => Err(error),
        }
    }

    fn lock_for(&self, key: PathBuf) -> Arc<tokio::sync::Mutex<()>> {
        let mut guard = self.queues.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = guard.get(&key).and_then(Weak::upgrade) {
            return existing;
        }
        let mutex = Arc::new(tokio::sync::Mutex::new(()));
        guard.insert(key, Arc::downgrade(&mutex));
        mutex
    }

    /// Execute `op` exclusively for the canonical path resolved from `file_path` and `cwd`,
    /// mapping canonicalization errors with `map_err`.
    pub async fn run_with<T, E, M, F, Fut>(
        &self,
        file_path: &str,
        cwd: &Path,
        map_err: M,
        op: F,
    ) -> Result<T, E>
    where
        M: FnOnce(String) -> E,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let resolved = resolve_to_cwd(file_path, cwd);
        let key = Self::canonical_key(&resolved)
            .await
            .map_err(|error| map_err(error.to_string()))?;
        let file_mutex = self.lock_for(key.clone());
        let _permit = file_mutex.lock().await;
        let result = op().await;
        drop(_permit);
        drop(file_mutex);
        {
            let mut guard = self.queues.lock().unwrap_or_else(|e| e.into_inner());
            if guard.get(&key).is_some_and(|weak| weak.strong_count() == 0) {
                guard.remove(&key);
            }
        }
        result
    }

    /// Execute `op` exclusively for the canonical path resolved from `file_path` and `cwd`.
    pub async fn run<T, F, Fut, E>(&self, file_path: &str, cwd: &Path, op: F) -> Result<T, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: From<String>,
    {
        self.run_with(file_path, cwd, E::from, op).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn serializes_same_path_and_allows_distinct_paths_in_parallel() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let queue = Arc::new(FileMutationQueue::new());
        let active_same = Arc::new(AtomicUsize::new(0));
        let max_same = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..4 {
            let q = queue.clone();
            let d = dir.path().to_path_buf();
            let active = active_same.clone();
            let max = max_same.clone();
            tasks.push(tokio::spawn(async move {
                q.run("shared.txt", &d, || async move {
                    let cur = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max.fetch_max(cur, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok::<_, String>(())
                })
                .await
            }));
        }
        for task in tasks {
            task.await.map_err(|e| e.to_string())??;
        }
        assert_eq!(max_same.load(Ordering::SeqCst), 1);

        let active_distinct = Arc::new(AtomicUsize::new(0));
        let max_distinct = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut distinct_tasks = Vec::new();
        for name in ["a.txt", "b.txt"] {
            let q = queue.clone();
            let d = dir.path().to_path_buf();
            let active = active_distinct.clone();
            let max = max_distinct.clone();
            let b = barrier.clone();
            distinct_tasks.push(tokio::spawn(async move {
                q.run(name, &d, || async move {
                    let cur = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max.fetch_max(cur, Ordering::SeqCst);
                    b.wait().await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok::<_, String>(())
                })
                .await
            }));
        }
        for task in distinct_tasks {
            task.await.map_err(|e| e.to_string())??;
        }
        assert_eq!(max_distinct.load(Ordering::SeqCst), 2);
        Ok(())
    }
}
