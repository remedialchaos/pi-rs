use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

#[cfg(unix)]
pub mod channel;
#[cfg(unix)]
pub mod supervisor;

pub const DEV_REBUILD_CMD_ENV: &str = "P2_DEV_REBUILD_CMD";
pub const DEV_SUPERVISOR_ENV: &str = "P2_DEV_SUPERVISOR";
pub const DEV_RESTART_EXIT_CODE: u8 = 75;

#[derive(Clone, Debug)]
pub struct DevRebuildRequest {
    pub data_dir: PathBuf,
    pub plugins_dir: PathBuf,
    pub plugins: Vec<String>,
}

pub type DevRebuildHook = Arc<
    dyn Fn(DevRebuildRequest) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>>
        + Send
        + Sync,
>;

/// Build a shell-based rebuild hook for `P2_DEV_REBUILD_CMD`, passing `P2_DATA_DIR`,
/// `P2_PLUGINS_DIR`, and `P2_RELOAD_PLUGINS`.
pub fn shell_rebuild_hook(command: String) -> DevRebuildHook {
    Arc::new(move |request| {
        let command = command.clone();
        Box::pin(async move {
            #[cfg(unix)]
            let mut cmd = {
                let mut cmd = tokio::process::Command::new("sh");
                cmd.arg("-c").arg(&command);
                cmd
            };
            #[cfg(not(unix))]
            let mut cmd = {
                let mut cmd = tokio::process::Command::new("cmd");
                cmd.arg("/C").arg(&command);
                cmd
            };
            let output = cmd
                .env("P2_DATA_DIR", &request.data_dir)
                .env("P2_PLUGINS_DIR", &request.plugins_dir)
                .env("P2_RELOAD_PLUGINS", request.plugins.join(","))
                .output()
                .await
                .map_err(|error| format!("Rebuild command failed to start: {error}"))?;
            if output.status.success() {
                return Ok(());
            }
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            let detail = if !stderr.is_empty() {
                stderr
            } else if !stdout.is_empty() {
                stdout
            } else {
                format!("exit status {}", output.status)
            };
            Err(format!("Rebuild command failed: {detail}"))
        })
    })
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum DevCommand {
    Restart,
    Reload {
        #[serde(skip_serializing_if = "Option::is_none")]
        plugins: Option<Vec<String>>,
    },
}

pub fn read_command(body: &[u8]) -> Result<DevCommand, &'static str> {
    if body.len() > 16384 {
        return Err("Dev command too large");
    }
    let value: Value = serde_json::from_slice(body).map_err(|_| "Invalid dev command JSON")?;
    match value.get("action") {
        None => Err("Invalid dev command"),
        Some(action) if action == "restart" => Ok(DevCommand::Restart),
        Some(action) if action == "reload" => {
            let plugins = value
                .get("plugins")
                .map(|v| -> Result<Vec<String>, &'static str> {
                    let names = v
                        .as_array()
                        .filter(|a| a.len() <= 100)
                        .ok_or("Invalid plugin names")?;
                    let mut unique = indexmap::IndexSet::new();
                    for name in names {
                        let name = name.as_str().ok_or("Invalid plugin names")?;
                        if name.is_empty()
                            || name.len() > 100
                            || !name.bytes().enumerate().all(|(i, b)| {
                                b.is_ascii_lowercase()
                                    || b.is_ascii_digit()
                                    || i > 0 && matches!(b, b'-' | b'_')
                            })
                        {
                            return Err("Invalid plugin names");
                        }
                        unique.insert(name.to_owned());
                    }
                    Ok(unique.into_iter().collect())
                })
                .transpose()?;
            Ok(DevCommand::Reload { plugins })
        }
        _ => Err("Invalid dev action"),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DevResult {
    pub ok: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<Vec<String>>,
}

#[derive(Default)]
pub struct LoadedModules {
    processes: BTreeMap<String, BTreeMap<PathBuf, String>>,
    plugins: BTreeMap<String, BTreeSet<PathBuf>>,
}
// Node's path.resolve removes parent segments even when a loaded file was deleted.
fn resolve_file(file: &str) -> Option<PathBuf> {
    let absolute = std::path::absolute(file).ok()?;
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        if component == std::path::Component::ParentDir {
            resolved.pop();
        } else {
            resolved.push(component);
        }
    }
    Some(resolved)
}
impl LoadedModules {
    pub fn receive(&mut self, process: &str, report: &Value, roots: &[PathBuf]) {
        if report["type"] != "dev-modules" {
            return;
        }
        if let Some(entries) = report["modules"].as_array() {
            let parsed: Option<BTreeMap<_, _>> = entries
                .iter()
                .map(|e| {
                    let hash = e["hash"].as_str()?;
                    if hash.len() != 64
                        || !hash
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    {
                        return None;
                    }
                    Some((resolve_file(e["file"].as_str()?)?, hash.to_owned()))
                })
                .collect();
            if let Some(mut modules) = parsed {
                modules.retain(|file, _| roots.iter().any(|root| file.starts_with(root)));
                self.processes.insert(process.to_owned(), modules);
            }
        }
        if let Some(entries) = report["pluginFiles"].as_array()
            && let Some(files) = entries.iter().map(|v| resolve_file(v.as_str()?)).collect()
        {
            self.plugins.insert(process.to_owned(), files);
        }
    }
    pub fn delete(&mut self, process: &str) {
        self.processes.remove(process);
        self.plugins.remove(process);
    }
    pub fn plugin_files(&self) -> BTreeSet<&PathBuf> {
        self.plugins.values().flatten().collect()
    }
    pub fn changed(&self) -> BTreeSet<&Path> {
        let mut current = BTreeMap::new();
        let mut changed = BTreeSet::new();
        for (file, hash) in self.processes.values().flatten() {
            let disk = current.entry(file).or_insert_with(|| {
                std::fs::read(file)
                    .ok()
                    .map(|bytes| format!("{:x}", Sha256::digest(bytes)))
            });
            if disk.as_ref() != Some(hash) {
                changed.insert(file.as_path());
            }
        }
        changed
    }
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DevStatus {
    pub revision: u64,
    pub phase: Phase,
    pub plugins: Vec<String>,
    pub dirty: Vec<String>,
    pub restart_needed: bool,
    pub restart_files: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice_time: Option<u128>,
}
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    #[default]
    Idle,
    Reloading,
    Ready,
    Error,
}

#[derive(Default)]
pub struct DevControl {
    state: tokio::sync::Mutex<DevStatus>,
    active: tokio::sync::Mutex<()>,
    closed: std::sync::atomic::AtomicBool,
}

impl DevControl {
    /// Supply dirty, install, and selection changes together; restart paths are project-relative.
    pub async fn status(&self, dirty: BTreeSet<String>, restart_files: Vec<String>) -> DevStatus {
        let mut state = self.state.lock().await;
        let dirty: Vec<_> = dirty.into_iter().collect();
        if state.dirty != dirty {
            state.dirty = dirty;
            state.revision += 1;
        }
        if state.restart_files != restart_files {
            state.restart_needed = !restart_files.is_empty();
            state.restart_files = restart_files;
            state.revision += 1;
        }
        state.clone()
    }
    pub async fn notify(&self, result: &DevResult, redact: impl FnOnce(&str) -> String) {
        let mut state = self.state.lock().await;
        let message = redact(&result.message);
        state.phase = if result.ok {
            Phase::Ready
        } else {
            Phase::Error
        };
        state.plugins = result.plugins.clone().unwrap_or_default();
        state.message = Some(message);
        state.notice_id = Some(uuid::Uuid::new_v4().to_string());
        state.notice_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|time| time.as_millis());
        state.revision += 1;
    }
    pub async fn reload<T, Fut>(
        &self,
        requested: Option<Vec<String>>,
        dirty: BTreeSet<String>,
        apply: impl FnOnce(Vec<String>) -> Fut,
        redact: impl FnOnce(&str) -> String,
    ) -> Result<T, String>
    where
        Fut: Future<Output = Result<(Vec<String>, T), String>>,
    {
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("Dev server is closing".into());
        }
        let Ok(_active) = self.active.try_lock() else {
            let message = "A dev command is already running";
            self.notify(
                &DevResult {
                    ok: false,
                    message: format!("Reload refused: {message}"),
                    plugins: None,
                },
                redact,
            )
            .await;
            return Err(message.into());
        };
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("Dev server is closing".into());
        }
        let dirty: Vec<_> = dirty.into_iter().collect();
        let names = requested
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| dirty.clone());
        {
            let mut state = self.state.lock().await;
            state.phase = Phase::Reloading;
            state.dirty = dirty;
            state.revision += 1;
        }
        match apply(names.clone()).await {
            Ok((plugins, value)) => {
                let message = if plugins.is_empty() {
                    "No dirty plugins".into()
                } else {
                    format!("Reloaded {}", plugins.join(", "))
                };
                self.notify(
                    &DevResult {
                        ok: true,
                        message,
                        plugins: Some(plugins),
                    },
                    redact,
                )
                .await;
                Ok(value)
            }
            Err(error) => {
                self.notify(
                    &DevResult {
                        ok: false,
                        message: format!("Reload refused: {error}"),
                        plugins: Some(names),
                    },
                    redact,
                )
                .await;
                Err(error)
            }
        }
    }
    pub async fn restart<Fut>(
        &self,
        trigger: impl FnOnce() -> Fut,
        redact: impl FnOnce(&str) -> String,
    ) -> Result<(), String>
    where
        Fut: Future<Output = Result<(), String>>,
    {
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("Dev server is closing".into());
        }
        let Ok(_active) = self.active.try_lock() else {
            let message = "A dev command is already running";
            self.notify(
                &DevResult {
                    ok: false,
                    message: format!("Restart refused: {message}"),
                    plugins: None,
                },
                redact,
            )
            .await;
            return Err(message.into());
        };
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("Dev server is closing".into());
        }
        {
            let mut state = self.state.lock().await;
            state.phase = Phase::Reloading;
            state.revision += 1;
        }
        match trigger().await {
            Ok(()) => {
                self.notify(
                    &DevResult {
                        ok: true,
                        message: "Restarting server".into(),
                        plugins: None,
                    },
                    redact,
                )
                .await;
                Ok(())
            }
            Err(error) => {
                self.notify(
                    &DevResult {
                        ok: false,
                        message: format!("Restart refused: {error}"),
                        plugins: None,
                    },
                    redact,
                )
                .await;
                Err(error)
            }
        }
    }
    pub async fn close(&self) {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        drop(self.active.lock().await);
    }
}

impl LoadedModules {
    /// Refresh immediately before status and plugin queries. Supply the host PID probe.
    pub fn refresh_reports(
        &mut self,
        directory: &Path,
        roots: &[PathBuf],
        alive: impl Fn(u32) -> bool,
    ) {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        let mut live = BTreeSet::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let parsed = name
                .to_str()
                .and_then(|name| name.strip_suffix(".json"))
                .and_then(|name| name.split_once('-'))
                .and_then(|(parent, pid)| {
                    Some((parent.parse::<u32>().ok()?, pid.parse::<u32>().ok()?))
                });
            let Some((_, pid)) = parsed
                .filter(|(parent, pid)| *parent == std::process::id() && *pid != 0 && alive(*pid))
            else {
                let _ = std::fs::remove_file(entry.path());
                continue;
            };
            let key = format!("worker:{pid}");
            if let Ok(bytes) = std::fs::read(entry.path())
                && let Ok(report) = serde_json::from_slice(&bytes)
            {
                self.receive(&key, &report, roots);
            }
            live.insert(key);
        }
        self.processes
            .retain(|key, _| !key.starts_with("worker:") || live.contains(key));
        self.plugins
            .retain(|key, _| !key.starts_with("worker:") || live.contains(key));
    }
}

/// Per-process snapshot. The loader records only project/workspace inputs and
/// marks replaceable facet copies separately. The first loaded bytes stay authoritative.
#[derive(Default)]
pub struct ModuleReport {
    modules: indexmap::IndexMap<PathBuf, String>,
    plugin_files: indexmap::IndexSet<PathBuf>,
}
impl ModuleReport {
    pub fn record(&mut self, file: PathBuf, source: &[u8]) {
        self.modules
            .entry(file)
            .or_insert_with(|| format!("{:x}", Sha256::digest(source)));
    }
    pub fn record_plugin(&mut self, file: PathBuf) {
        self.plugin_files.insert(file);
    }
    pub fn message(&self) -> Value {
        let modules: Vec<_> = self
            .modules
            .iter()
            .map(|(file, hash)| serde_json::json!({"file":file,"hash":hash}))
            .collect();
        let files: Vec<_> = self.plugin_files.iter().collect();
        serde_json::json!({"type":"dev-modules","modules":modules,"pluginFiles":files})
    }
}

pub struct WorkerReport(PathBuf);
impl WorkerReport {
    pub fn new(directory: &Path, server_pid: u32) -> std::io::Result<Self> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(directory)?;
        Ok(Self(
            directory.join(format!("{server_pid}-{}.json", std::process::id())),
        ))
    }
    pub fn publish(&self, report: &ModuleReport) -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        serde_json::to_writer(options.open(&self.0)?, &report.message())
            .map_err(std::io::Error::other)
    }
}
impl Drop for WorkerReport {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
