//! Discover p2's immutable compiled facets and optional user components for one host.
use crate::api::v1::{FacetKind, PluginRegistry};
use pi_rs_chord::{
    Result,
    context::Context,
    host::{Host, PluginFacet, ReloadReport},
    node::plugins::{DiscoveryProblem, PluginRoot, PluginRuntime, PluginScript, discover_plugins},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

pub struct PluginSources {
    builtins: BTreeSet<String>,
    selected: Vec<String>,
    native: Vec<(PluginFacet, PluginScript)>,
    root: PluginRoot,
    kind: FacetKind,
    active: BTreeMap<String, PluginScript>,
    origins: BTreeMap<String, PathBuf>,
}

pub struct PluginUpdate {
    pub selected: Vec<String>,
    pub problems: Vec<DiscoveryProblem>,
    pub report: ReloadReport,
}

struct Scan {
    selected: BTreeSet<String>,
    scripts: BTreeMap<String, PluginScript>,
    origins: BTreeMap<String, PathBuf>,
    problems: Vec<DiscoveryProblem>,
}

impl PluginSources {
    /// Selection applies only to compiled built-ins. Every built-in reserves its name in every kind.
    pub fn new(
        registry: &PluginRegistry,
        selection: Option<&[String]>,
        user_root: &Path,
        kind: FacetKind,
    ) -> Result<Self> {
        let selected = registry.select(selection)?;
        let native = registry
            .facets(kind, &selected)
            .into_iter()
            .map(|entry| {
                // Compiled facets cannot change within this process.
                let mut script = PluginScript::native(
                    &entry.name,
                    uuid::Uuid::new_v4().as_bytes(),
                    entry.facet.clone(),
                );
                script.required = true;
                (entry, script)
            })
            .collect();
        Ok(Self {
            builtins: registry.select(None)?.into_iter().collect(),
            selected,
            native,
            root: PluginRoot {
                path: user_root.to_owned(),
                required: false,
            },
            kind,
            active: BTreeMap::new(),
            origins: BTreeMap::new(),
        })
    }

    /// Changed closures get fresh revisions. Cloned facets retain their digest and live objects.
    pub fn replace_registry(
        &mut self,
        registry: &PluginRegistry,
        requested: Option<&[String]>,
    ) -> Result<()> {
        let mut next = Self::new(registry, requested, &self.root.path, self.kind)?;
        for (facet, script) in &mut next.native {
            if let Some((_, old)) = self
                .native
                .iter()
                .find(|(old, _)| old.name == facet.name && old.facet.same(&facet.facet))
            {
                *script = old.clone();
            }
        }
        next.active = std::mem::take(&mut self.active);
        next.origins = std::mem::take(&mut self.origins);
        *self = next;
        Ok(())
    }

    fn scan(&self, previous: &BTreeMap<String, PathBuf>) -> Result<Scan> {
        let mut found = discover_plugins(
            std::slice::from_ref(&self.root),
            &["server", "worker", "web", "tui"],
        )?;
        let mut selected: BTreeSet<_> = self.selected.iter().cloned().collect();
        let mut origins = BTreeMap::new();
        let mut problems = std::mem::take(&mut found.problems);
        let discovery_errors = problems.len();
        found.plugins.retain(|plugin| {
            if plugin.facets.is_empty() {
                return false;
            }
            if self.builtins.contains(&plugin.name) {
                problems.push(DiscoveryProblem {
                    path: plugin.dir.clone(),
                    required: false,
                    error: format!("plugin {} is already discovered", plugin.name),
                });
                return false;
            }
            selected.insert(plugin.name.clone());
            if let Some(file) = plugin.facets.get(self.kind.as_str()) {
                origins.insert(
                    plugin.name.clone(),
                    if plugin.dir == plugin.root {
                        file.clone()
                    } else {
                        plugin.dir.clone()
                    },
                );
            }
            true
        });
        let mut scripts: BTreeMap<_, _> = self
            .native
            .iter()
            .map(|(_, script)| script.clone())
            .chain(PluginScript::from_discovery(&found, self.kind.as_str()))
            .map(|s| (s.name.clone(), s))
            .collect();
        // A broken manifest is a rejected replacement. An actual deletion has no discovery problem.
        for (name, origin) in previous {
            if !scripts.contains_key(name)
                && let Some(problem) = problems[..discovery_errors]
                    .iter()
                    .find(|p| p.path.starts_with(origin) || origin.starts_with(&p.path))
            {
                scripts.insert(
                    name.clone(),
                    PluginScript::failed(name, &problem.error, false),
                );
                selected.insert(name.clone());
                origins.insert(name.clone(), origin.clone());
            }
        }
        Ok(Scan {
            selected,
            scripts,
            origins,
            problems,
        })
    }

    /// Replaces the initial host.load call. Include the host's complete core facet list.
    pub async fn load(
        &mut self,
        host: Host,
        core: Vec<PluginFacet>,
        ctx: &Context,
    ) -> Result<(PluginRuntime, PluginUpdate)> {
        let scan = self.scan(&BTreeMap::new())?;
        let (runtime, report) =
            PluginRuntime::load(host, core, scan.scripts.values().cloned().collect(), ctx).await?;
        self.active = scan.scripts;
        self.active.retain(|name, _| runtime.digest(name).is_some());
        self.origins = scan.origins;
        Ok((
            runtime,
            PluginUpdate {
                selected: scan.selected.into_iter().collect(),
                problems: scan.problems,
                report,
            },
        ))
    }

    /// Rediscover the root on every explicit reload. Chord retains the last good facet on rejection.
    pub async fn reload(&mut self, runtime: &PluginRuntime, ctx: &Context) -> Result<PluginUpdate> {
        self.reload_named(runtime, &[], ctx).await
    }

    pub async fn reload_named(
        &mut self,
        runtime: &PluginRuntime,
        requested: &[String],
        ctx: &Context,
    ) -> Result<PluginUpdate> {
        let mut scan = self.scan(&self.origins)?;
        if !requested.is_empty() {
            for name in requested {
                if !scan.selected.contains(name)
                    && !self.active.contains_key(name)
                    && !self.origins.contains_key(name)
                {
                    let reason = if self.builtins.contains(name) {
                        "Cannot reload unloaded p2 plugin"
                    } else {
                        "Unknown p2 plugin"
                    };
                    return Err(pi_rs_chord::Error::Invalid(format!("{reason}: {name}")));
                }
            }
            scan.scripts.retain(|name, _| requested.contains(name));
            scan.origins.retain(|name, _| requested.contains(name));
            scan.scripts.extend(
                self.active
                    .iter()
                    .filter(|(name, _)| !requested.contains(name))
                    .map(|(k, v)| (k.clone(), v.clone())),
            );
            scan.origins.extend(
                self.origins
                    .iter()
                    .filter(|(name, _)| !requested.contains(name))
                    .map(|(k, v)| (k.clone(), v.clone())),
            );
        }
        let report = runtime
            .reload(scan.scripts.values().cloned().collect(), ctx)
            .await?;
        for failure in &report.failed {
            if let Some(previous) = self.active.get(&failure.plugin) {
                scan.scripts
                    .insert(failure.plugin.clone(), previous.clone());
            } else {
                scan.scripts.remove(&failure.plugin);
            }
        }
        self.active = scan.scripts;
        self.origins = scan.origins;
        Ok(PluginUpdate {
            selected: scan.selected.into_iter().collect(),
            problems: scan.problems,
            report,
        })
    }
}

/// Snapshot the worker bytes in the supervisor, before the child opens its store and reports Ready.
pub fn worker_scripts(
    data_dir: &Path,
    reserved: &BTreeSet<String>,
) -> Result<Vec<pi_rs_chord::node::workers::DeliveredPlugin>> {
    use pi_rs_chord::node::{plugins::digest, workers::DeliveredPlugin};
    let root = PluginRoot {
        path: crate::runtime::host_dirs::user_plugins_directory(data_dir),
        required: false,
    };
    let found = discover_plugins(&[root], &["worker"])?;
    for problem in found.problems {
        eprintln!("p2 plugins: {}: {}", problem.path.display(), problem.error);
    }
    let mut scripts = Vec::new();
    for plugin in found.plugins {
        let Some(path) = plugin.facets.get("worker") else {
            continue;
        };
        if reserved.contains(&plugin.name) {
            eprintln!(
                "p2 plugins: {}: plugin {} is already discovered",
                path.display(),
                plugin.name
            );
            continue;
        }
        match std::fs::read(path) {
            Ok(bytes) => scripts.push(DeliveredPlugin {
                name: plugin.name,
                digest: digest(&bytes),
                bytes,
            }),
            Err(error) => eprintln!("p2 plugins: {}: {error}", path.display()),
        }
    }
    Ok(scripts)
}
impl PluginUpdate {
    pub fn log_problems(&self) {
        for p in &self.problems {
            eprintln!("p2 plugins: {}: {}", p.path.display(), p.error);
        }
        for f in &self.report.failed {
            eprintln!("p2 plugins: {}: {}", f.plugin, f.error);
        }
    }
}
