//! Portable surface contracts and client state. Browser and launcher effects are supplied by adapters.
use crate::{
    Error, Result,
    context::{Context, lock},
    contracts::{Contract, Member, schema::*},
    host::{Facet, Host, PluginFacet, PluginFailure, ReloadReport},
    json::JsonValue,
    state::{Batch, NativeSource},
};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
pub mod connection;
pub mod format;
pub mod language;
pub use format::{NumberFormatOptions, TimeFormatOptions, TimeValue, format_number, format_time};
mod preference;
pub use preference::*;
pub mod themes;
pub use connection::*;
pub use language::*;
pub use themes::*;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptEntry {
    pub name: String,
    pub digest: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub css: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptSet {
    pub base: String,
    pub combined: String,
    pub plugins: Vec<ScriptEntry>,
}
pub fn scripts_contract(name: &str) -> Result<Contract> {
    let entry = obj([
        ("name", required(str())),
        ("digest", required(str())),
        ("url", required(str())),
        ("css", opt(str())),
    ]);
    let set = obj([
        ("base", required(str())),
        ("combined", required(str())),
        ("plugins", required(arr(entry))),
    ]);
    Contract::remote(
        name,
        1,
        vec![],
        IndexMap::from([(
            "main".into(),
            IndexMap::from([("set".into(), Member::State(set))]),
        )]),
        Arc::new(|_| {
            crate::contracts::HostKind::new(
                "server",
                crate::contracts::read_params(&[], "server")?,
            )?
            .address(JsonValue::Object(IndexMap::new()))
        }),
    )
}
pub fn web_scripts() -> Result<Contract> {
    scripts_contract("chord.web.scripts")
}
pub fn tui_scripts() -> Result<Contract> {
    scripts_contract("chord.tui.scripts")
}
pub fn registered_source(plugin: &str, digest: &str, source: &str) -> Result<String> {
    Ok(format!(
        "chordFacet({},{},{}\n);\n",
        serde_json::to_string(plugin)?,
        serde_json::to_string(digest)?,
        source
    ))
}
#[derive(Clone)]
pub struct PreparedFacet {
    pub facet: Facet,
    pub digest: String,
}
pub struct SetEntry {
    pub name: String,
    pub digest: Option<String>,
    pub prepare: Arc<dyn Fn() -> Result<PreparedFacet> + Send + Sync>,
}
pub struct LoadedFacets {
    host: Host,
    core: Vec<PluginFacet>,
    loaded: Arc<Mutex<BTreeMap<String, PreparedFacet>>>,
    line: Arc<tokio::sync::Mutex<()>>,
}

/// A staged reload of [`LoadedFacets`], ready to commit or abort.
pub struct StagedLoadedFacets {
    loaded: Arc<Mutex<BTreeMap<String, PreparedFacet>>>,
    _line: tokio::sync::OwnedMutexGuard<()>,
    next: BTreeMap<String, PreparedFacet>,
    staged: crate::host::StagedReload,
}

impl StagedLoadedFacets {
    pub fn failed(&self) -> &[PluginFailure] {
        self.staged.failed()
    }

    pub fn has_failures(&self) -> bool {
        self.staged.has_failures()
    }

    pub fn abort(self) {
        self.staged.abort();
    }

    pub async fn commit(self, context: &Context) -> Result<ReloadReport> {
        let StagedLoadedFacets {
            loaded,
            _line,
            mut next,
            staged,
        } = self;
        let report = staged.commit(context).await?;
        for failure in &report.failed {
            if let Some(previous) = lock(&loaded).get(&failure.plugin).cloned() {
                next.insert(failure.plugin.clone(), previous);
            } else {
                next.remove(&failure.plugin);
            }
        }
        *lock(&loaded) = next;
        Ok(report)
    }
}

impl LoadedFacets {
    pub async fn load(
        host: Host,
        core: Vec<PluginFacet>,
        entries: Vec<SetEntry>,
        context: &Context,
    ) -> Result<(Self, ReloadReport)> {
        let mut prepared = BTreeMap::new();
        let mut failed = Vec::new();
        let mut facets = core.clone();
        for entry in entries {
            match (entry.prepare)() {
                Ok(value) => {
                    facets.push(PluginFacet::new(&entry.name, value.facet.clone()));
                    prepared.insert(entry.name, value);
                }
                Err(error) => failed.push(PluginFailure {
                    plugin: entry.name,
                    error: error.to_string(),
                }),
            }
        }
        let mut report = host.load(facets, context).await?;
        for failure in &report.failed {
            prepared.remove(&failure.plugin);
        }
        report.failed.extend(failed);
        Ok((
            Self {
                host,
                core,
                loaded: Arc::new(Mutex::new(prepared)),
                line: Arc::new(tokio::sync::Mutex::new(())),
            },
            report,
        ))
    }
    pub fn digest(&self, name: &str) -> Option<String> {
        lock(&self.loaded).get(name).map(|p| p.digest.clone())
    }
    pub async fn stage_reload(
        &self,
        entries: Vec<SetEntry>,
        context: &Context,
    ) -> Result<StagedLoadedFacets> {
        let line = self.line.clone().lock_owned().await;
        let mut next = BTreeMap::new();
        let mut failed = Vec::new();
        for entry in entries {
            let previous = lock(&self.loaded).get(&entry.name).cloned();
            if let Some(previous) = previous
                .as_ref()
                .filter(|p| entry.digest.as_ref() == Some(&p.digest))
            {
                next.insert(entry.name, previous.clone());
                continue;
            }
            match (entry.prepare)() {
                Ok(prepared) => {
                    next.insert(entry.name, prepared);
                }
                Err(error) => {
                    failed.push(PluginFailure {
                        plugin: entry.name.clone(),
                        error: error.to_string(),
                    });
                    if let Some(previous) = previous {
                        next.insert(entry.name, previous);
                    }
                }
            }
        }
        let facets = self
            .core
            .iter()
            .cloned()
            .chain(
                next.iter()
                    .map(|(name, p)| PluginFacet::new(name, p.facet.clone())),
            )
            .collect();
        let staged = self.host.stage_reload(facets, failed, context).await?;
        Ok(StagedLoadedFacets {
            loaded: self.loaded.clone(),
            _line: line,
            next,
            staged,
        })
    }
    pub async fn reload(&self, entries: Vec<SetEntry>, context: &Context) -> Result<ReloadReport> {
        self.stage_reload(entries, context)
            .await?
            .commit(context)
            .await
    }
}
/// Host-owned values survive facet replacement, but not host shutdown.
#[derive(Default)]
pub struct PageStates {
    values: Mutex<BTreeMap<String, Arc<dyn std::any::Any + Send + Sync>>>,
}
impl PageStates {
    pub fn get<T: Clone + PartialEq + Send + 'static>(
        &self,
        key: &str,
        initial: T,
        batch: Batch,
    ) -> Result<NativeSource<T>> {
        let mut values = lock(&self.values);
        if let Some(value) = values.get(key) {
            return value
                .downcast_ref::<NativeSource<T>>()
                .cloned()
                .ok_or_else(|| Error::Invalid(format!("page state {key}: type changed")));
        }
        let source = NativeSource::new(batch, initial, |a, b| a == b);
        values.insert(key.into(), Arc::new(source.clone()));
        Ok(source)
    }
}
#[derive(Clone, Default)]
pub struct Preferences {
    values: Arc<Mutex<IndexMap<String, JsonValue>>>,
}
impl Preferences {
    pub fn pick(&self, principal: &serde_json::Value, none: JsonValue) -> Result<JsonValue> {
        let key = crate::data::principal_key(principal)?;
        Ok(lock(&self.values).get(&key).cloned().unwrap_or(none))
    }
    pub fn choose(&self, principal: &serde_json::Value, value: JsonValue) -> Result<()> {
        let key = crate::data::principal_key(principal)?;
        let mut picks = lock(&self.values);
        picks.shift_remove(&key);
        picks.insert(key, value);
        while picks.len() > 10_000 {
            picks.shift_remove_index(0);
        }
        Ok(())
    }
}
pub fn live_in(state: &crate::host::RefState, current: Option<&str>, variant: &str) -> bool {
    matches!(state, crate::host::RefState::Live) && current == Some(variant)
}
