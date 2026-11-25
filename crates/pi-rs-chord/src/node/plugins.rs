//! Compiled-in Rust facets and digest-addressed WASM component plugins.
use crate::{
    Error, Result,
    context::Context,
    host::{Facet, Host, PluginFacet, PluginFailure, ReloadReport},
    surfaces::{LoadedFacets, PreparedFacet, SetEntry, StagedLoadedFacets},
};
use indexmap::IndexMap;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
mod wasm;
pub use wasm::{WasmPluginHost, bindings};

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
#[derive(Clone, Debug)]
pub struct PluginRoot {
    pub path: PathBuf,
    pub required: bool,
}
#[derive(Clone, Debug)]
pub struct Plugin {
    pub name: String,
    pub dir: PathBuf,
    pub root: PathBuf,
    pub required: bool,
    pub facets: IndexMap<String, PathBuf>,
}
#[derive(Clone, Debug)]
pub struct DiscoveryProblem {
    pub path: PathBuf,
    pub error: String,
    pub required: bool,
}
#[derive(Clone, Debug, Default)]
pub struct Discovery {
    pub plugins: Vec<Plugin>,
    pub problems: Vec<DiscoveryProblem>,
}

pub fn discover_plugins(roots: &[PluginRoot], kinds: &[&str]) -> Result<Discovery> {
    let mut found = BTreeMap::<String, Plugin>::new();
    let mut problems = Vec::new();
    for root in roots {
        let path = match std::fs::canonicalize(&root.path) {
            Ok(path) => path,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !root.required => continue,
            Err(e) => {
                problems.push(DiscoveryProblem {
                    path: root.path.clone(),
                    error: e.to_string(),
                    required: root.required,
                });
                continue;
            }
        };
        let mut entries = std::fs::read_dir(&path)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| (!entry.path().is_dir(), entry.file_name()));
        let mut grouped = BTreeMap::<String, Plugin>::new();
        for entry in entries {
            let filename = entry.file_name().to_string_lossy().into_owned();
            if filename.starts_with('.') || filename == "node_modules" {
                continue;
            }
            let file = entry.path();
            if file.is_dir() {
                let manifest = file.join("package.json");
                if !manifest.is_file() {
                    problems.push(DiscoveryProblem {
                        path: file,
                        error: "no package.json".into(),
                        required: root.required,
                    });
                    continue;
                }
                let package: serde_json::Value =
                    match serde_json::from_slice(&std::fs::read(&manifest)?) {
                        Ok(value) => value,
                        Err(e) => {
                            problems.push(DiscoveryProblem {
                                path: manifest,
                                error: e.to_string(),
                                required: root.required,
                            });
                            continue;
                        }
                    };
                if let Some(listed) = package.pointer("/chord/plugins") {
                    let folders = listed.as_array().ok_or_else(|| {
                        Error::Invalid("chord.plugins is not a list of plugin folders".into())
                    });
                    match folders {
                        Ok(folders) => {
                            for folder in folders {
                                let result = (|| -> Result<()> {
                                    let folder = folder.as_str().ok_or_else(|| {
                                        Error::Invalid("chord.plugins paths must be strings".into())
                                    })?;
                                    if folder != "." {
                                        super::app::safe_relative(folder)?;
                                    }
                                    let dir = std::fs::canonicalize(file.join(folder))?;
                                    if !dir.starts_with(std::fs::canonicalize(&file)?) {
                                        return Err(Error::Invalid(
                                            "plugin folder leaves package".into(),
                                        ));
                                    }
                                    let manifest: serde_json::Value = serde_json::from_slice(
                                        &std::fs::read(dir.join("package.json"))?,
                                    )?;
                                    let name =
                                        manifest.get("name").and_then(|n| n.as_str()).ok_or_else(
                                            || Error::Invalid("plugin has no package name".into()),
                                        )?;
                                    discover_directory(
                                        name,
                                        &dir,
                                        &path,
                                        root.required,
                                        kinds,
                                        &mut grouped,
                                    )
                                })();
                                if let Err(error) = result {
                                    problems.push(DiscoveryProblem {
                                        path: file.clone(),
                                        error: error.to_string(),
                                        required: root.required,
                                    });
                                }
                            }
                        }
                        Err(error) => problems.push(DiscoveryProblem {
                            path: file.clone(),
                            error: error.to_string(),
                            required: root.required,
                        }),
                    }
                } else {
                    let name = package.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    if let Err(e) =
                        discover_directory(name, &file, &path, root.required, kinds, &mut grouped)
                    {
                        problems.push(DiscoveryProblem {
                            path: file,
                            error: e.to_string(),
                            required: root.required,
                        });
                    }
                }
            } else if let Some(stem) = filename.strip_suffix(".wasm") {
                let Some((name, kind)) = stem.rsplit_once('.') else {
                    continue;
                };
                if !kinds.contains(&kind) {
                    continue;
                }
                let real = std::fs::canonicalize(&file)?;
                if !real.starts_with(&path) || grouped.get(name).is_some_and(|p| p.dir != path) {
                    problems.push(DiscoveryProblem {
                        path: file,
                        error: "facet escapes root or duplicates a directory plugin".into(),
                        required: root.required,
                    });
                    continue;
                }
                if let Err(e) = crate::contracts::check_name(name, "plugin name") {
                    problems.push(DiscoveryProblem {
                        path: file,
                        error: e.to_string(),
                        required: root.required,
                    });
                    continue;
                }
                let plugin = grouped.entry(name.into()).or_insert_with(|| Plugin {
                    name: name.into(),
                    dir: path.clone(),
                    root: path.clone(),
                    required: root.required,
                    facets: IndexMap::new(),
                });
                if plugin.facets.insert(kind.into(), real).is_some() {
                    problems.push(DiscoveryProblem {
                        path: plugin.dir.clone(),
                        error: format!("duplicate facet {kind}"),
                        required: root.required,
                    });
                }
            }
        }
        for (name, plugin) in grouped {
            match found.entry(name) {
                std::collections::btree_map::Entry::Occupied(entry) => {
                    problems.push(DiscoveryProblem {
                        path: plugin.dir,
                        error: format!("plugin {} is already discovered", entry.key()),
                        required: root.required,
                    })
                }
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(plugin);
                }
            }
        }
    }
    Ok(Discovery {
        plugins: found.into_values().collect(),
        problems,
    })
}
fn discover_directory(
    name: &str,
    dir: &Path,
    root: &Path,
    required: bool,
    kinds: &[&str],
    found: &mut BTreeMap<String, Plugin>,
) -> Result<()> {
    crate::contracts::check_name(name, "plugin name")?;
    if found.contains_key(name) {
        return Err(Error::Invalid(format!("duplicate plugin {name}")));
    }
    let dir = std::fs::canonicalize(dir)?;
    if !dir.starts_with(root) {
        return Err(Error::Invalid("plugin directory escapes its root".into()));
    }
    let mut facets = IndexMap::new();
    for kind in kinds {
        let file = dir.join(format!("{kind}.wasm"));
        if file.is_file() {
            let file = std::fs::canonicalize(file)?;
            if !file.starts_with(root) {
                return Err(Error::Invalid("plugin facet escapes its root".into()));
            }
            facets.insert((*kind).into(), file);
        }
    }
    if found
        .insert(
            name.into(),
            Plugin {
                name: name.into(),
                dir,
                root: root.into(),
                required,
                facets,
            },
        )
        .is_some()
    {
        return Err(Error::Invalid(format!("duplicate plugin {name}")));
    }
    Ok(())
}
#[derive(Clone)]
enum ScriptBody {
    Native(Facet),
    Wasm(Arc<[u8]>),
    Failed(String),
}
#[derive(Clone)]
pub struct PluginScript {
    pub name: String,
    pub required: bool,
    digest: String,
    body: ScriptBody,
}
impl PluginScript {
    pub fn native(name: impl Into<String>, revision: &[u8], facet: Facet) -> Self {
        Self {
            name: name.into(),
            required: false,
            digest: digest(revision),
            body: ScriptBody::Native(facet),
        }
    }
    pub fn wasm(name: impl Into<String>, bytes: Vec<u8>, required: bool) -> Self {
        Self {
            name: name.into(),
            required,
            digest: digest(&bytes),
            body: ScriptBody::Wasm(bytes.into()),
        }
    }
    pub fn failed(name: impl Into<String>, error: impl Into<String>, required: bool) -> Self {
        Self {
            name: name.into(),
            required,
            digest: String::new(),
            body: ScriptBody::Failed(error.into()),
        }
    }
    pub fn digest(&self) -> &str {
        &self.digest
    }
    pub fn from_discovery(discovery: &Discovery, kind: &str) -> Vec<Self> {
        discovery
            .plugins
            .iter()
            .filter_map(|p| {
                p.facets.get(kind).map(|path| match std::fs::read(path) {
                    Ok(bytes) => Self::wasm(&p.name, bytes, p.required),
                    Err(e) => Self::failed(&p.name, e.to_string(), p.required),
                })
            })
            .collect()
    }
}
pub struct PluginRuntime {
    loaded: LoadedFacets,
    wasm: WasmPluginHost,
    line: Arc<tokio::sync::Mutex<()>>,
}

/// A staged plugin reload on a single [`PluginRuntime`], ready to commit or abort.
pub struct StagedPluginReload {
    _line: tokio::sync::OwnedMutexGuard<()>,
    staged: StagedLoadedFacets,
}

impl StagedPluginReload {
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
        self.staged.commit(context).await
    }
}

impl PluginRuntime {
    pub async fn load(
        host: Host,
        core: Vec<PluginFacet>,
        scripts: Vec<PluginScript>,
        context: &Context,
    ) -> Result<(Self, ReloadReport)> {
        let wasm = WasmPluginHost::new()?;
        let entries = prepare_scripts(&wasm, None, &scripts, context).await?;
        let (loaded, report) = LoadedFacets::load(host, core, entries, context).await?;
        let runtime = Self {
            loaded,
            wasm,
            line: Arc::new(tokio::sync::Mutex::new(())),
        };
        if report
            .failed
            .iter()
            .any(|f| scripts.iter().any(|s| s.name == f.plugin && s.required))
        {
            return Err(Error::Invalid(format!(
                "required plugins failed: {:?}",
                report.failed
            )));
        }
        Ok((runtime, report))
    }
    pub fn digest(&self, name: &str) -> Option<String> {
        self.loaded.digest(name)
    }
    pub async fn stage_reload(
        &self,
        scripts: Vec<PluginScript>,
        context: &Context,
    ) -> Result<StagedPluginReload> {
        let line = self.line.clone().lock_owned().await;
        let entries = prepare_scripts(&self.wasm, Some(&self.loaded), &scripts, context).await?;
        let staged = self.loaded.stage_reload(entries, context).await?;
        Ok(StagedPluginReload {
            _line: line,
            staged,
        })
    }
    pub async fn reload(
        &self,
        scripts: Vec<PluginScript>,
        context: &Context,
    ) -> Result<ReloadReport> {
        self.stage_reload(scripts, context)
            .await?
            .commit(context)
            .await
    }
}
async fn prepare_scripts(
    wasm: &WasmPluginHost,
    loaded: Option<&LoadedFacets>,
    scripts: &[PluginScript],
    context: &Context,
) -> Result<Vec<SetEntry>> {
    let mut entries = Vec::new();
    for script in scripts {
        context.check().map_err(|e| Error::Invalid(e.to_string()))?;
        let digest = script.digest.clone();
        let facet =
            if loaded.is_some_and(|loaded| loaded.digest(&script.name).as_ref() == Some(&digest)) {
                // LoadedFacets never calls prepare for an unchanged digest.
                Err("unchanged digest was unexpectedly prepared".into())
            } else {
                match &script.body {
                    ScriptBody::Native(facet) => Ok(facet.clone()),
                    ScriptBody::Wasm(bytes) => wasm.prepare(bytes).await.map_err(|e| e.to_string()),
                    ScriptBody::Failed(error) => Err(error.clone()),
                }
            };
        entries.push(SetEntry {
            name: script.name.clone(),
            digest: Some(digest.clone()),
            prepare: Arc::new(move || match &facet {
                Ok(facet) => Ok(PreparedFacet {
                    facet: facet.clone(),
                    digest: digest.clone(),
                }),
                Err(error) => Err(Error::Invalid(error.clone())),
            }),
        });
    }
    Ok(entries)
}

/// Real OS notifications, coalesced until there have been no events for `debounce`.
pub struct PluginWatcher {
    watcher: Option<notify::RecommendedWatcher>,
    receiver: tokio::sync::mpsc::Receiver<notify::Result<notify::Event>>,
    debounce: Duration,
    overflow: Arc<std::sync::atomic::AtomicBool>,
}
impl PluginWatcher {
    pub fn new(roots: &[PathBuf], debounce: Duration) -> Result<Self> {
        use notify::Watcher;
        let (sender, receiver) = tokio::sync::mpsc::channel(256);
        let overflow = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let overloaded = overflow.clone();
        let mut watcher = notify::recommended_watcher(move |event| {
            if sender.try_send(event).is_err() {
                overloaded.store(true, std::sync::atomic::Ordering::Release);
            }
        })
        .map_err(|e| Error::Invalid(e.to_string()))?;
        for root in roots {
            watcher
                .watch(root, notify::RecursiveMode::Recursive)
                .map_err(|e| Error::Invalid(e.to_string()))?;
        }
        Ok(Self {
            watcher: Some(watcher),
            receiver,
            debounce,
            overflow,
        })
    }
    pub async fn changed(&mut self) -> Result<Vec<PathBuf>> {
        let first = self.receiver.recv().await.ok_or(Error::Closed)?;
        let mut paths = std::collections::BTreeSet::new();
        let mut event = Some(first);
        while let Some(next) = event {
            let next = next.map_err(|e| Error::Invalid(e.to_string()))?;
            if !matches!(next.kind, notify::EventKind::Access(_)) {
                paths.extend(next.paths);
            }
            event = tokio::time::timeout(self.debounce, self.receiver.recv())
                .await
                .unwrap_or_default();
        }
        if self
            .overflow
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            return Err(Error::Invalid(
                "plugin watcher queue overflow, rediscover all plugins".into(),
            ));
        }
        Ok(paths.into_iter().collect())
    }
    pub fn close(&mut self) {
        self.watcher.take();
        self.receiver.close();
    }
}

pub struct RolloutStep<'a> {
    pub name: String,
    pub runtime: &'a PluginRuntime,
    pub scripts: Vec<PluginScript>,
}
/// Reload runtimes in order, with independent publication at each host.
/// A rejected plugin is held back in its host's report without preventing
/// later steps. An error from the reload itself stops the rollout; earlier
/// steps are not rolled back.
pub async fn rollout(
    steps: Vec<RolloutStep<'_>>,
    context: &Context,
) -> Result<Vec<(String, ReloadReport)>> {
    let mut reports = Vec::new();
    for step in steps {
        reports.push((step.name, step.runtime.reload(step.scripts, context).await?));
    }
    Ok(reports)
}

/// Result of a two-phase coordinated reload across multiple [`PluginRuntime`] participants.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CoordinatedRolloutReport {
    pub committed: bool,
    pub participants: Vec<(String, ReloadReport)>,
}

/// Two-phase coordinated reload across multiple [`PluginRuntime`] participants.
/// Stages and validates every participant first; commits all only if every participant
/// stages with zero failures; otherwise aborts and holds back all participants.
pub async fn coordinated_rollout(
    steps: Vec<RolloutStep<'_>>,
    context: &Context,
) -> Result<CoordinatedRolloutReport> {
    for (idx, step) in steps.iter().enumerate() {
        if steps[idx + 1..]
            .iter()
            .any(|other| std::ptr::eq(step.runtime, other.runtime))
        {
            return Err(Error::Invalid("duplicate rollout participant".into()));
        }
    }
    let mut staged = Vec::with_capacity(steps.len());
    for step in steps {
        match step.runtime.stage_reload(step.scripts, context).await {
            Ok(participant) => staged.push((step.name, participant)),
            Err(error) => {
                for (_, participant) in staged {
                    participant.abort();
                }
                return Err(error);
            }
        }
    }
    if staged
        .iter()
        .any(|(_, participant)| participant.has_failures())
    {
        let mut participants = Vec::with_capacity(staged.len());
        for (name, participant) in staged {
            let report = ReloadReport {
                failed: participant.failed().to_vec(),
                ..ReloadReport::default()
            };
            participant.abort();
            participants.push((name, report));
        }
        return Ok(CoordinatedRolloutReport {
            committed: false,
            participants,
        });
    }
    let mut participants = Vec::with_capacity(staged.len());
    for (name, participant) in staged {
        let report = participant.commit(context).await?;
        participants.push((name, report));
    }
    Ok(CoordinatedRolloutReport {
        committed: true,
        participants,
    })
}

/// Ergonomic builder for two-phase coordinated multi-host plugin reloads.
#[derive(Default)]
pub struct ReloadCoordinator<'a> {
    steps: Vec<RolloutStep<'a>>,
}

impl<'a> ReloadCoordinator<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(
        mut self,
        name: impl Into<String>,
        runtime: &'a PluginRuntime,
        scripts: Vec<PluginScript>,
    ) -> Self {
        self.steps.push(RolloutStep {
            name: name.into(),
            runtime,
            scripts,
        });
        self
    }

    pub async fn execute(self, context: &Context) -> Result<CoordinatedRolloutReport> {
        coordinated_rollout(self.steps, context).await
    }
}
