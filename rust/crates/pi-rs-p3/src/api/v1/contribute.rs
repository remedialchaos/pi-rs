//! `contribute`: what a facet contributes to a registry lives from the moment the facet is set up until
//! the plugin's life ends (the server's close, a plugin's restart).
//!
//! Upstream serves a host-lifetime object named for the contribution and calls `register(self)`; its
//! `on_live` routine runs when the object is live and its abort runs when it ends. Native facets have
//! no dynamic objects, so the equivalent is a [`PluginLive`] scope: contributions are recorded in the
//! base's typed tables ([`crate::registry`]) together with the plugin that made them, work that must
//! outlive the setup is a task the scope aborts, and [`PluginLives::end_all`] withdraws everything when
//! the server closes. A contribution that needs nothing at run time is the facet's own table entry and
//! needs no scope.

use std::{
    future::Future,
    sync::{Arc, Mutex},
};

use tokio::task::AbortHandle;

use crate::registry::Plugins;

/// One plugin's contributions and tasks, withdrawn together when its life ends.
#[derive(Default)]
pub struct PluginLive {
    plugin: String,
    cleanups: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
    tasks: Mutex<Vec<AbortHandle>>,
}

impl PluginLive {
    pub fn new(plugin: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            plugin: plugin.into(),
            cleanups: Mutex::new(Vec::new()),
            tasks: Mutex::new(Vec::new()),
        })
    }

    pub fn plugin(&self) -> &str {
        &self.plugin
    }

    /// Contributes `value` to `table` for this plugin, withdrawn when its life ends.
    pub fn contribute<T: Clone + Send + Sync + 'static>(&self, table: &Plugins<T>, value: T) {
        table.set(&self.plugin, value);
        let table = table.clone();
        let plugin = self.plugin.clone();
        self.cleanup(move || table.remove(&plugin));
    }

    /// Registers a withdrawal, run in reverse order when the life ends.
    pub fn cleanup(&self, cleanup: impl FnOnce() + Send + 'static) {
        self.cleanups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Box::new(cleanup));
    }

    /// Runs `future` for this plugin, aborted when its life ends.
    pub fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        let task = tokio::spawn(future);
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(task.abort_handle());
    }

    /// Withdraws every contribution and stops every task of this plugin. Ending twice is harmless.
    pub fn end(&self) {
        let tasks = std::mem::take(
            &mut *self
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for task in tasks {
            task.abort();
        }
        let cleanups = std::mem::take(
            &mut *self
                .cleanups
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for cleanup in cleanups.into_iter().rev() {
            cleanup();
        }
    }
}

/// Every plugin's life on one host.
#[derive(Default)]
pub struct PluginLives {
    lives: Mutex<Vec<Arc<PluginLive>>>,
}

impl PluginLives {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A new life for `plugin`, ended with the rest.
    pub fn open(self: &Arc<Self>, plugin: impl Into<String>) -> Arc<PluginLive> {
        let live = PluginLive::new(plugin);
        self.lives
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(live.clone());
        live
    }

    /// Ends every plugin's life.
    pub fn end_all(&self) {
        let lives = std::mem::take(
            &mut *self
                .lives
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for live in lives {
            live.end();
        }
    }
}

/// Contributes `value` to `table` as `live`'s plugin: the native form of upstream's
/// `contribute(host, name, register)`.
pub fn contribute<T: Clone + Send + Sync + 'static>(
    live: &PluginLive,
    table: &Plugins<T>,
    value: T,
) {
    live.contribute(table, value);
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[test]
    fn ending_a_plugin_withdraws_what_it_contributed() {
        let table: Plugins<&str> = Plugins::new();
        let live = PluginLive::new("p3-probe");
        contribute(&live, &table, "probe");
        assert_eq!(table.first(), Some("probe"));
        live.end();
        assert_eq!(table.first(), None);
    }

    #[test]
    fn withdrawals_run_in_reverse_order_once() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let live = PluginLive::new("p3-probe");
        for step in ["first", "second"] {
            let order = order.clone();
            live.cleanup(move || {
                order
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(step);
            });
        }
        live.end();
        live.end();
        assert_eq!(
            *order
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            ["second", "first"]
        );
    }

    #[tokio::test]
    async fn ending_a_plugin_stops_its_tasks() {
        let ran = Arc::new(AtomicBool::new(false));
        let lives = PluginLives::new();
        let live = lives.open("p3-probe");
        let flag = ran.clone();
        live.spawn(async move {
            flag.store(true, Ordering::Release);
        });
        tokio::task::yield_now().await;
        assert!(ran.load(Ordering::Acquire));
        lives.end_all();

        let survived = Arc::new(AtomicBool::new(false));
        let live = lives.open("p3-probe");
        let flag = survived.clone();
        live.spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                flag.store(true, Ordering::Release);
            }
        });
        lives.end_all();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!survived.load(Ordering::Acquire));
    }
}
