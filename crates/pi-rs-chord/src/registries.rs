//! Owner-scoped registry tables and atomic, observable compositions.
use crate::{
    Error, Result,
    context::{Context, lock},
    state::{Batch, NativeObservation, NativeSource},
    watch::NativeWatchCall,
};
use indexmap::IndexMap;
use std::{
    collections::{BTreeMap, BTreeSet},
    marker::PhantomData,
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug)]
pub struct Registry<C, V> {
    pub name: String,
    pub version: u64,
    types: PhantomData<fn(C) -> V>,
}
pub fn define_registry<C, V>(name: impl Into<String>, version: u64) -> Result<Registry<C, V>> {
    let name = name.into();
    crate::contracts::check_name(&name, "registry")?;
    if version == 0 || version > 9_007_199_254_740_991 {
        return Err(Error::Invalid(format!(
            "{name}.version: expected a positive integer"
        )));
    }
    Ok(Registry {
        name,
        version,
        types: PhantomData,
    })
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Contributor {
    pub plugin: String,
    pub object_type: Option<String>,
}
impl Contributor {
    pub fn facet(plugin: impl Into<String>) -> Self {
        Self {
            plugin: plugin.into(),
            object_type: None,
        }
    }
    pub fn object(plugin: impl Into<String>, object_type: impl Into<String>) -> Self {
        Self {
            plugin: plugin.into(),
            object_type: Some(object_type.into()),
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct RegistryEntry<C> {
    pub plugin: String,
    pub object_type: Option<String>,
    pub version: u64,
    pub value: C,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryConflict {
    pub message: String,
    pub plugins: Vec<String>,
}
#[derive(Default)]
pub struct ComposeContext {
    pub conflicts: Vec<RegistryConflict>,
}
impl ComposeContext {
    pub fn report(&mut self, conflict: RegistryConflict) {
        if !self.conflicts.contains(&conflict) {
            self.conflicts.push(conflict);
        }
    }
}
type Adapter<T> = Arc<dyn Fn(&T) -> Result<T> + Send + Sync>;
type Compose<C, V> =
    Arc<dyn Fn(&[RegistryEntry<C>], &mut ComposeContext) -> Result<V> + Send + Sync>;
#[derive(Clone)]
pub struct RegistryProvider<C, V> {
    compose: Compose<C, V>,
    accepts: BTreeMap<u64, (Adapter<C>, Adapter<V>)>,
}
impl<C, V> RegistryProvider<C, V> {
    pub fn new(
        compose: impl Fn(&[RegistryEntry<C>], &mut ComposeContext) -> Result<V> + Send + Sync + 'static,
    ) -> Self {
        Self {
            compose: Arc::new(compose),
            accepts: BTreeMap::new(),
        }
    }
    pub fn accept(
        mut self,
        version: u64,
        contribution: impl Fn(&C) -> Result<C> + Send + Sync + 'static,
        value: impl Fn(&V) -> Result<V> + Send + Sync + 'static,
    ) -> Result<Self> {
        if version == 0 {
            return Err(Error::Invalid(
                "registry adapter version is positive".into(),
            ));
        }
        self.accepts
            .insert(version, (Arc::new(contribution), Arc::new(value)));
        Ok(self)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistryError {
    Undeclared {
        registry: String,
        plugins: Vec<String>,
    },
    Refused {
        registry: String,
        contributor: Contributor,
        version: u64,
    },
    ContributionFailed {
        registry: String,
        contributor: Contributor,
        error: String,
    },
    Failed {
        registry: String,
        error: String,
    },
    Conflict {
        registry: String,
        conflict: RegistryConflict,
    },
}
struct Contribution<C> {
    version: u64,
    value: C,
    failure: Option<String>,
    adapted: Option<C>,
    observation: Option<NativeObservation<C>>,
}
struct State<C, V> {
    provider: Option<RegistryProvider<C, V>>,
    contributions: BTreeMap<Contributor, Contribution<C>>,
    table: Vec<RegistryEntry<C>>,
    good: Option<V>,
    good_owners: BTreeSet<Contributor>,
    failure: Option<String>,
    composing: bool,
    closed: bool,
    episodes: BTreeSet<String>,
    conflicts: Vec<RegistryConflict>,
    mapped: BTreeMap<u64, V>,
}
struct Inner<C, V> {
    token: Registry<C, V>,
    version: std::sync::atomic::AtomicU64,
    batch: Batch,
    state: Mutex<State<C, V>>,
    value: NativeSource<Option<V>>,
    report: Arc<dyn Fn(RegistryError) + Send + Sync>,
}
#[derive(Clone)]
pub struct RegistryRecord<C, V>(Arc<Inner<C, V>>);
impl<C, V> RegistryRecord<C, V>
where
    C: Clone + PartialEq + Send + 'static,
    V: Clone + PartialEq + Send + 'static,
{
    pub fn new(
        token: Registry<C, V>,
        batch: Batch,
        report: impl Fn(RegistryError) + Send + Sync + 'static,
    ) -> Self {
        let value = NativeSource::new(batch.clone(), None, |a, b| a == b);
        Self(Arc::new(Inner {
            version: std::sync::atomic::AtomicU64::new(token.version),
            token,
            batch,
            value,
            report: Arc::new(report),
            state: Mutex::new(State {
                provider: None,
                contributions: BTreeMap::new(),
                table: Vec::new(),
                good: None,
                good_owners: BTreeSet::new(),
                failure: None,
                composing: false,
                closed: false,
                episodes: BTreeSet::new(),
                conflicts: Vec::new(),
                mapped: BTreeMap::new(),
            }),
        }))
    }
    pub fn source(&self) -> NativeSource<Option<V>> {
        self.0.value.clone()
    }
    pub fn last_good(&self) -> Option<V> {
        lock(&self.0.state).good.clone()
    }
    pub fn value(&self, version: u64) -> Result<V> {
        let state = lock(&self.0.state);
        if state.composing {
            return Err(crate::data::ChordError::invalid(
                "cycle",
                format!("registry {} is read while it composes", self.0.token.name),
                None,
            )
            .into());
        }
        if state.closed {
            return state.good.clone().ok_or(Error::Closed);
        }
        let provider = state
            .provider
            .clone()
            .ok_or_else(|| unavailable(&self.0.token.name))?;
        if let Some(error) = &state.failure {
            return Err(Error::Invalid(error.clone()));
        }
        let value = state
            .good
            .clone()
            .ok_or_else(|| unavailable(&self.0.token.name))?;
        if version == self.version() {
            return Ok(value);
        }
        if let Some(mapped) = state.mapped.get(&version) {
            return Ok(mapped.clone());
        }
        let adapter = provider
            .accepts
            .get(&version)
            .filter(|_| version < self.version())
            .map(|a| a.1.clone())
            .ok_or_else(|| {
                crate::data::ChordError::new(
                    "contract_mismatch",
                    format!(
                        "registry {}: version {version} is not served",
                        self.0.token.name
                    ),
                )
            })?;
        drop(state);
        let mapped = self.0.batch.compute(|| adapter(&value))?;
        lock(&self.0.state).mapped.insert(version, mapped.clone());
        Ok(mapped)
    }
    pub fn table(&self) -> Result<Vec<RegistryEntry<C>>> {
        let state = lock(&self.0.state);
        if state.composing {
            return Err(Error::Invalid("registry cycle".into()));
        }
        Ok(state.table.clone())
    }
    pub fn serve(&self, provider: RegistryProvider<C, V>, context: &Context) -> Result<()> {
        if provider.accepts.keys().any(|v| *v >= self.version()) {
            return Err(Error::Invalid(
                "registry accepts only older versions".into(),
            ));
        }
        self.change(context, move |state| {
            state.provider = Some(provider);
            for c in state.contributions.values_mut() {
                c.adapted = None;
            }
        })
    }
    pub fn unserve(&self, context: &Context) -> Result<()> {
        self.change(context, |state| {
            state.provider = None;
            state.good = None;
            state.good_owners.clear();
        })
    }
    pub fn contribute(
        &self,
        contributor: Contributor,
        version: u64,
        value: C,
        context: &Context,
    ) -> Result<()> {
        self.change(context, move |state| {
            state.contributions.insert(
                contributor,
                Contribution {
                    version,
                    value,
                    failure: None,
                    adapted: None,
                    observation: None,
                },
            );
        })
    }
    pub fn remove_owner(&self, contributor: &Contributor, context: &Context) -> Result<()> {
        let contributor = contributor.clone();
        self.change(context, move |state| {
            state.contributions.remove(&contributor);
        })
    }
    pub fn replace(
        &self,
        provider: RegistryProvider<C, V>,
        entries: Vec<(Contributor, u64, C)>,
        context: &Context,
    ) -> Result<()> {
        self.replace_versioned(self.version(), provider, entries, false, context)
    }
    pub(crate) fn replace_versioned(
        &self,
        version: u64,
        provider: RegistryProvider<C, V>,
        entries: Vec<(Contributor, u64, C)>,
        retain_objects: bool,
        context: &Context,
    ) -> Result<()> {
        if version == 0 || provider.accepts.keys().any(|v| *v >= version) {
            return Err(Error::Invalid(
                "registry accepts only older versions".into(),
            ));
        }
        let record = self.clone();
        self.change(context, move |state| {
            record
                .0
                .version
                .store(version, std::sync::atomic::Ordering::Release);
            state.provider = Some(provider);
            state.contributions.retain(|owner, entry| {
                entry.adapted = None;
                retain_objects && owner.object_type.is_some()
            });
            state
                .contributions
                .extend(entries.into_iter().map(|(owner, version, value)| {
                    (
                        owner,
                        Contribution {
                            version,
                            value,
                            failure: None,
                            adapted: None,
                            observation: None,
                        },
                    )
                }));
        })
    }
    fn version(&self) -> u64 {
        self.0.version.load(std::sync::atomic::Ordering::Acquire)
    }
    pub fn follow(
        &self,
        contributor: Contributor,
        version: u64,
        source: NativeSource<C>,
        context: &Context,
    ) -> Result<()> {
        self.contribute(contributor.clone(), version, source.value(), context)?;
        let weak = Arc::downgrade(&self.0);
        let owner = contributor.clone();
        let observation = source.observe(
            context,
            move |call, ctx| {
                if let Some(inner) = weak.upgrade() {
                    let record = Self(inner);
                    let owner = owner.clone();
                    record.change(ctx, move |state| {
                        if let Some(entry) = state.contributions.get_mut(&owner) {
                            match call {
                                NativeWatchCall::Value { value } => {
                                    entry.value = value;
                                    entry.failure = None;
                                    entry.adapted = None;
                                }
                                NativeWatchCall::Failed { error } => entry.failure = Some(error),
                            }
                        }
                    })?;
                }
                Ok(())
            },
            |_| {},
        )?;
        if let Some(entry) = lock(&self.0.state).contributions.get_mut(&contributor) {
            entry.observation = Some(observation);
        }
        Ok(())
    }
    fn change(
        &self,
        context: &Context,
        change: impl FnOnce(&mut State<C, V>) + Send + 'static,
    ) -> Result<()> {
        self.0.batch.assert_not_computing("registry change")?;
        if lock(&self.0.state).closed {
            return Err(Error::Closed);
        }
        let record = self.clone();
        let ctx = context.clone();
        self.0.batch.within(context, move || {
            change(&mut lock(&record.0.state));
            record.recompose(&ctx)
        })
    }
    fn recompose(&self, context: &Context) -> Result<()> {
        let mut state = lock(&self.0.state);
        state.mapped.clear();
        if !state
            .good_owners
            .iter()
            .all(|o| state.contributions.contains_key(o))
        {
            state.good = None;
        }
        let Some(provider) = state.provider.clone() else {
            state.failure = Some("unavailable/undeclared".into());
            let plugins = state
                .contributions
                .keys()
                .map(|c| c.plugin.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            drop(state);
            self.0.value.fail("unavailable/undeclared", context)?;
            if !plugins.is_empty() {
                (self.0.report)(RegistryError::Undeclared {
                    registry: self.0.token.name.clone(),
                    plugins,
                });
            }
            return Ok(());
        };
        if state.composing {
            return Err(Error::Invalid("registry cycle".into()));
        }
        state.composing = true;
        let contributions = state
            .contributions
            .iter()
            .map(|(o, c)| {
                (
                    o.clone(),
                    c.version,
                    c.value.clone(),
                    c.failure.clone(),
                    c.adapted.clone(),
                )
            })
            .collect::<Vec<_>>();
        drop(state);
        let mut episodes = BTreeSet::new();
        let mut reports = Vec::new();
        let mut table = Vec::new();
        for (owner, version, raw, failure, cached) in contributions {
            let value = if let Some(error) = failure {
                Err(error)
            } else if version == self.version() {
                Ok(raw)
            } else if version < self.version() {
                match provider.accepts.get(&version) {
                    Some((adapt, _)) => match cached {
                        Some(v) => Ok(v),
                        None => self
                            .0
                            .batch
                            .compute(|| adapt(&raw))
                            .map_err(|e| e.to_string()),
                    },
                    None => {
                        episodes.insert(format!("refused:{owner:?}"));
                        reports.push(RegistryError::Refused {
                            registry: self.0.token.name.clone(),
                            contributor: owner,
                            version,
                        });
                        continue;
                    }
                }
            } else {
                episodes.insert(format!("refused:{owner:?}"));
                reports.push(RegistryError::Refused {
                    registry: self.0.token.name.clone(),
                    contributor: owner,
                    version,
                });
                continue;
            };
            match value {
                Ok(value) => {
                    if let Some(c) = lock(&self.0.state).contributions.get_mut(&owner) {
                        c.adapted = Some(value.clone());
                    }
                    table.push(RegistryEntry {
                        plugin: owner.plugin,
                        object_type: owner.object_type,
                        version,
                        value,
                    });
                }
                Err(error) => {
                    episodes.insert(format!("failed:{owner:?}"));
                    reports.push(RegistryError::ContributionFailed {
                        registry: self.0.token.name.clone(),
                        contributor: owner,
                        error,
                    });
                }
            }
        }
        let mut compose_context = ComposeContext::default();
        let result = self
            .0
            .batch
            .compute(|| (provider.compose)(&table, &mut compose_context));
        let mut state = lock(&self.0.state);
        state.composing = false;
        state.table = table;
        let previous = std::mem::replace(&mut state.episodes, episodes);
        reports.retain(|report| {
            let key = match report {
                RegistryError::Refused { contributor, .. } => format!("refused:{contributor:?}"),
                RegistryError::ContributionFailed { contributor, .. } => {
                    format!("failed:{contributor:?}")
                }
                _ => String::new(),
            };
            !previous.contains(&key)
        });
        let publish = match result {
            Ok(value) => {
                state.good = Some(value.clone());
                state.good_owners = state.contributions.keys().cloned().collect();
                state.failure = None;
                for conflict in &compose_context.conflicts {
                    if !state.conflicts.contains(conflict) {
                        reports.push(RegistryError::Conflict {
                            registry: self.0.token.name.clone(),
                            conflict: conflict.clone(),
                        });
                    }
                }
                state.conflicts = compose_context.conflicts;
                Ok(value)
            }
            Err(error) => {
                let error = error.to_string();
                if state.failure.is_none() {
                    reports.push(RegistryError::Failed {
                        registry: self.0.token.name.clone(),
                        error: error.clone(),
                    });
                }
                state.failure = Some(error.clone());
                Err(error)
            }
        };
        drop(state);
        match publish {
            Ok(value) => self.0.value.set(Some(value), context)?,
            Err(error) => self.0.value.fail(error, context)?,
        }
        for report in reports {
            (self.0.report)(report);
        }
        Ok(())
    }
    pub fn close(&self) {
        let mut state = lock(&self.0.state);
        state.closed = true;
        state.contributions.clear();
        drop(state);
        self.0.value.detach("released/closed");
    }
}
fn unavailable(name: &str) -> Error {
    let mut error = crate::data::ChordError::new(
        "unavailable",
        format!("registry {name}: no plugin serves it here"),
    );
    error.reason = Some("undeclared".into());
    error.into()
}
#[derive(Clone, Debug, PartialEq)]
pub struct TableItem<T> {
    pub value: T,
    pub plugin: String,
    pub object_type: Option<String>,
}
pub fn keyed_entries<C, I>(
    table: &[RegistryEntry<C>],
    entries: impl Fn(&C) -> Vec<(String, I)>,
    context: &mut ComposeContext,
) -> IndexMap<String, TableItem<I>> {
    let mut out: IndexMap<String, TableItem<I>> = IndexMap::new();
    for entry in table {
        for (key, value) in entries(&entry.value) {
            if let Some(taken) = out.get(&key) {
                context.report(RegistryConflict {
                    message: format!("\"{key}\" is contributed twice"),
                    plugins: vec![taken.plugin.clone(), entry.plugin.clone()],
                });
                continue;
            }
            out.insert(
                key,
                TableItem {
                    value,
                    plugin: entry.plugin.clone(),
                    object_type: entry.object_type.clone(),
                },
            );
        }
    }
    out
}
pub fn ordered_entries<C, I>(
    table: &[RegistryEntry<C>],
    items: impl Fn(&C) -> Vec<I>,
    order: impl Fn(&I) -> i64,
) -> Vec<TableItem<I>> {
    let mut out = table
        .iter()
        .flat_map(|e| {
            items(&e.value).into_iter().map(|value| TableItem {
                value,
                plugin: e.plugin.clone(),
                object_type: e.object_type.clone(),
            })
        })
        .collect::<Vec<_>>();
    out.sort_by_key(|i| order(&i.value));
    out
}
