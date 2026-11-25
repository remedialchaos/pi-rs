//! p3's registries, held by the base and contributed to by plugins: the server's process values
//! (`ServerRuntime`), how a session's worker runs (`WorkerRunners`), hooks on accepted links
//! (`LinkOpened`), and a worker's own values (`WorkerRuntime`, `WorkerBusy`).
//!
//! `pi-rs-chord`'s registry records carry JSON contributions; p3's are typed native values, so each
//! table here is keyed by plugin name and composed when it is read: the first entry by plugin name,
//! all entries in plugin order, or their sum. This is the local seam for the typed registry adapter
//! `pi-rs-chord` does not have yet (see `MILESTONE.md`).

use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};

use pi_rs_chord::node::http::HttpRequest;

use crate::{runner::WorkerRunner, server::ServerRuntime, worker::WorkerRuntime};

/// A hook on each `/chord` link the HTTP component accepts: the hook returns what to call once the
/// link closed.
pub type LinkOpenedHook =
    Arc<dyn Fn(&HttpRequest) -> Option<Box<dyn FnOnce() + Send>> + Send + Sync>;

/// One plugin's contribution to a table.
#[derive(Clone)]
pub struct PluginEntry<T> {
    pub plugin: String,
    pub value: T,
}

/// A table of contributions, keyed by plugin name and read in plugin order.
#[derive(Clone)]
pub struct Plugins<T> {
    entries: Arc<RwLock<BTreeMap<String, T>>>,
}

impl<T> Default for Plugins<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Plugins<T> {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    /// Contributes `value` for `plugin`, replacing any earlier contribution of that plugin.
    pub fn set(&self, plugin: &str, value: T) {
        self.write().insert(plugin.to_string(), value);
    }

    /// Removes `plugin`'s contribution, if it has one.
    pub fn remove(&self, plugin: &str) {
        self.write().remove(plugin);
    }

    /// Whether the table holds nothing.
    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }
}

impl<T: Clone> Plugins<T> {
    /// The first contribution, by plugin name: the one the route uses.
    pub fn first(&self) -> Option<T> {
        self.read().values().next().cloned()
    }

    /// Every contribution, in plugin order.
    pub fn all(&self) -> Vec<T> {
        self.read().values().cloned().collect()
    }

    /// Every contribution with the plugin that made it, in plugin order.
    pub fn entries(&self) -> Vec<PluginEntry<T>> {
        self.read()
            .iter()
            .map(|(plugin, value)| PluginEntry {
                plugin: plugin.clone(),
                value: value.clone(),
            })
            .collect()
    }
}

impl<T> Plugins<T> {
    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, T>> {
        self.entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<String, T>> {
        self.entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The server's tables.
#[derive(Clone, Default)]
pub struct ServerRegistries {
    pub server_runtime: Plugins<Arc<ServerRuntime>>,
    pub worker_runners: Plugins<Arc<dyn WorkerRunner>>,
    pub link_opened: Plugins<LinkOpenedHook>,
}

impl ServerRegistries {
    /// The server's process values, once its base facet is live.
    pub fn runtime(&self) -> Option<Arc<ServerRuntime>> {
        self.server_runtime.first()
    }

    /// The runner the route to session workers uses: the first contribution by plugin name.
    pub fn runner(&self) -> Option<Arc<dyn WorkerRunner>> {
        self.worker_runners.first()
    }

    /// Every link hook, in plugin order.
    pub fn link_hooks(&self) -> Vec<LinkOpenedHook> {
        self.link_opened.all()
    }
}

/// The worker's tables.
#[derive(Clone, Default)]
pub struct WorkerRegistries {
    pub worker_runtime: Plugins<Arc<WorkerRuntime>>,
    pub worker_busy: Plugins<u64>,
    /// The sessions plugin's view of the server, when a worker's link reaches it (the `2.1b-3`
    /// seam; a worker process has none yet).
    pub session_facts: Plugins<Arc<dyn crate::plugins::sessions::SessionFactsLink>>,
}

impl WorkerRegistries {
    pub fn runtime(&self) -> Option<Arc<WorkerRuntime>> {
        self.worker_runtime.first()
    }

    /// The composed busy count: the sum of every contribution.
    pub fn busy(&self) -> u64 {
        self.worker_busy.all().into_iter().sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_contribution_is_the_one_by_plugin_name() {
        let plugins: Plugins<&str> = Plugins::new();
        plugins.set("p3-z", "z");
        plugins.set("p3-a", "a");
        assert_eq!(plugins.first(), Some("a"));
        plugins.remove("p3-a");
        assert_eq!(plugins.first(), Some("z"));
        assert_eq!(plugins.all(), ["z"]);
        plugins.remove("p3-z");
        assert!(plugins.is_empty());
        assert_eq!(plugins.first(), None);
    }

    #[test]
    fn a_plugin_replaces_its_own_contribution() {
        let plugins: Plugins<u64> = Plugins::new();
        plugins.set("p3-runner", 1);
        plugins.set("p3-probe", 2);
        plugins.set("p3-runner", 40);
        assert_eq!(plugins.entries().len(), 2);
        assert_eq!(plugins.entries()[0].plugin, "p3-probe");
        assert_eq!(plugins.entries()[1].value, 40);
    }

    #[test]
    fn the_worker_busy_count_is_the_sum() {
        let registries = WorkerRegistries::default();
        assert_eq!(registries.busy(), 0);
        registries.worker_busy.set("p3-runner", 2);
        registries.worker_busy.set("p3-harness", 3);
        assert_eq!(registries.busy(), 5);
        registries.worker_busy.remove("p3-runner");
        assert_eq!(registries.busy(), 3);
    }
}
