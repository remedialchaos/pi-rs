//! Project local or remote WIT facets' command metadata and panel state onto the terminal.
use super::when::When;
use crate::hosts::tui_bundles::{MAX_TOTAL_TUI_BUNDLES, MAX_TUI_BUNDLE, TuiManifest};
use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    context::Context,
    host::{Held, Host, HostOptions, PluginFailure, create_host},
    json::JsonValue,
    node::plugins::{PluginRoot, PluginRuntime, PluginScript, digest, discover_plugins},
    state::Subscription,
};
use pi_rs_p2_tui::{
    CORE_COMMANDS, RenderContext, ThemeRef,
    components::{PanelContent, PanelFrame},
    line_text,
};
use reqwest::{Client, Url, header};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;

#[derive(Clone, Deserialize)]
struct Command {
    #[serde(default, deserialize_with = "read_when")]
    when: When,
    slash: Option<Slash>,
    keys: Option<pi_rs_p2_tui::KeyIdValue>,
}

fn read_when<'de, D: serde::Deserializer<'de>>(input: D) -> std::result::Result<When, D::Error> {
    When::parse(&String::deserialize(input)?)
        .ok_or_else(|| serde::de::Error::custom("invalid command context expression"))
}

#[derive(Clone, Deserialize)]
struct Slash {
    name: String,
}

#[derive(Deserialize)]
struct Panel {
    id: String,
    title: String,
    lines: Vec<String>,
}

struct Lines(Vec<String>);
impl pi_rs_tui::Component for Lines {
    fn render(&self, _: usize) -> Vec<String> {
        self.0.clone()
    }
}
impl PanelContent for Lines {}

struct Style(ThemeRef);
impl RenderContext for Style {
    fn theme(&self) -> &ThemeRef {
        &self.0
    }
    fn key_text(&self, id: &str) -> String {
        if id == "p2.panel.focus" { "F2" } else { "Esc" }.into()
    }
    fn request_render(&self) {}
}

pub enum PanelUpdate {
    Unchanged,
    Close,
    Show(PanelFrame),
}

#[derive(Clone, Debug, Default)]
pub struct ReloadOutcome {
    pub active_removed: bool,
    pub failed: Vec<PluginFailure>,
}

#[derive(Clone)]
pub struct RemoteSource {
    pub client: Client,
    pub base_url: Url,
    pub origin: String,
    pub cookie: String,
    pub poll_interval: Duration,
}

struct Guest {
    name: String,
    object: Held,
    changed: Arc<AtomicBool>,
    commands: IndexMap<String, Command>,
    _subscription: Subscription,
}

impl Drop for Guest {
    fn drop(&mut self) {
        self.object.release();
    }
}

struct RemotePoller {
    rx: mpsc::Receiver<(String, Vec<PluginScript>)>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for RemotePoller {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub struct Plugins {
    host: Host,
    runtime: PluginRuntime,
    commands: IndexMap<String, (usize, Command)>,
    guests: Vec<Guest>,
    active: Option<usize>,
    remote: Option<RemoteSource>,
    last_version: Option<String>,
    poller: Option<RemotePoller>,
}

impl Plugins {
    pub async fn load(root: PathBuf) -> Result<Option<Self>> {
        let discovery = discover_plugins(
            &[PluginRoot {
                path: root,
                required: false,
            }],
            &["tui"],
        )?;
        let scripts = PluginScript::from_discovery(&discovery, "tui");
        if scripts.is_empty() {
            return Ok(None);
        }
        let plugins = Self::from_scripts(scripts, None, None).await?;
        Ok(Some(plugins))
    }

    pub async fn load_remote_or_local(
        remote: RemoteSource,
        local_fallback: Option<PathBuf>,
    ) -> Result<Option<Self>> {
        let mut cache = BTreeMap::new();
        match fetch_remote_manifest(&remote).await {
            Ok(Some(manifest)) => {
                let scripts = download_manifest_scripts(&remote, &manifest, &mut cache).await?;
                let mut plugins = Self::from_scripts(
                    scripts,
                    Some(remote.clone()),
                    Some(manifest.version.clone()),
                )
                .await?;
                plugins.start_poller(remote, manifest.version, cache);
                Ok(Some(plugins))
            }
            Ok(None) => match local_fallback {
                Some(root) => Self::load(root).await,
                None => Ok(None),
            },
            Err(error) => Err(error),
        }
    }

    async fn from_scripts(
        scripts: Vec<PluginScript>,
        remote: Option<RemoteSource>,
        last_version: Option<String>,
    ) -> Result<Self> {
        let ctx = Context::background();
        let host = create_host(HostOptions::default())?;
        let names: Vec<String> = scripts.iter().map(|s| s.name.clone()).collect();
        let (runtime, report) = PluginRuntime::load(host.clone(), vec![], scripts, &ctx).await?;
        if let Some(failed) = report.failed.first() {
            return Err(Error::Invalid(format!(
                "TUI facet {}: {}",
                failed.plugin, failed.error
            )));
        }
        let mut plugins = Self {
            host,
            runtime,
            commands: IndexMap::new(),
            guests: Vec::new(),
            active: None,
            remote,
            last_version,
            poller: None,
        };
        for name in names {
            let Some(guest) = Self::build_guest(&plugins.host, &name, &ctx).await? else {
                continue;
            };
            plugins.register_guest(guest);
        }
        Ok(plugins)
    }

    fn start_poller(
        &mut self,
        remote: RemoteSource,
        initial_version: String,
        mut cache: BTreeMap<String, Vec<u8>>,
    ) {
        let (tx, rx) = mpsc::channel(4);
        let interval = remote.poll_interval.max(Duration::from_millis(50));
        let task = tokio::spawn(async move {
            let mut current_version = initial_version;
            loop {
                tokio::time::sleep(interval).await;
                let Ok(Some(manifest)) = fetch_remote_manifest(&remote).await else {
                    continue;
                };
                if manifest.version == current_version {
                    continue;
                }
                match download_manifest_scripts(&remote, &manifest, &mut cache).await {
                    Ok(scripts) => {
                        current_version = manifest.version.clone();
                        if tx.send((manifest.version, scripts)).await.is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        eprintln!("p2 tui remote reload error: {error}");
                    }
                }
            }
        });
        self.poller = Some(RemotePoller { rx, task });
    }

    async fn build_guest(host: &Host, name: &str, ctx: &Context) -> Result<Option<Guest>> {
        let Some(contract) = host.served_contract(name) else {
            return Ok(None);
        };
        let object = host
            .get(contract, JsonValue::Object(Default::default()), ctx)
            .await?;
        let commands: IndexMap<String, Command> =
            serde_json::from_value(object.read("commands")?.to_serde()?)?;
        let changed = Arc::new(AtomicBool::new(false));
        let mark = changed.clone();
        let subscription = object.member("panel")?.subscribe(
            move |_, _| {
                mark.store(true, Ordering::Release);
                Ok(())
            },
            |_| {},
        );
        Ok(Some(Guest {
            name: name.to_owned(),
            object,
            changed,
            commands,
            _subscription: subscription,
        }))
    }

    fn register_guest(&mut self, guest: Guest) {
        let idx = self.guests.len();
        for (id, mut command) in guest.commands.clone() {
            if id.starts_with("tui.")
                || pi_rs_p2_tui::is_privileged(&id)
                || CORE_COMMANDS.iter().any(|c| c.id == id)
                || self.commands.contains_key(&id)
            {
                continue;
            }
            if let Some(slash) = &command.slash
                && (CORE_COMMANDS.iter().any(|c| c.slash == Some(&slash.name))
                    || self
                        .commands
                        .values()
                        .any(|(_, c)| c.slash.as_ref().is_some_and(|s| s.name == slash.name)))
            {
                command.slash = None;
            }
            self.commands.insert(id, (idx, command));
        }
        self.guests.push(guest);
    }

    pub async fn reload_from_scripts(
        &mut self,
        scripts: Vec<PluginScript>,
    ) -> Result<ReloadOutcome> {
        let ctx = Context::background();
        let active_name = self
            .active
            .and_then(|i| self.guests.get(i))
            .map(|g| g.name.clone());
        let ordered_names: Vec<String> = scripts.iter().map(|s| s.name.clone()).collect();
        let report = self.runtime.reload(scripts, &ctx).await?;

        let replaced: HashSet<String> = report
            .added
            .iter()
            .chain(report.changed.iter())
            .cloned()
            .collect();
        let failed: HashSet<String> = report.failed.iter().map(|f| f.plugin.clone()).collect();
        let mut previous: BTreeMap<String, Guest> =
            self.guests.drain(..).map(|g| (g.name.clone(), g)).collect();
        self.commands.clear();

        for name in ordered_names {
            if (!replaced.contains(&name) || failed.contains(&name))
                && let Some(guest) = previous.remove(&name)
            {
                self.register_guest(guest);
                continue;
            }
            if let Some(guest) = Self::build_guest(&self.host, &name, &ctx).await? {
                if active_name.as_deref() == Some(name.as_str()) {
                    guest.changed.store(true, Ordering::Release);
                }
                self.register_guest(guest);
            }
        }

        self.active = active_name
            .as_deref()
            .and_then(|name| self.guests.iter().position(|g| g.name == name));
        let active_removed = active_name.is_some() && self.active.is_none();

        Ok(ReloadOutcome {
            active_removed,
            failed: report.failed,
        })
    }

    pub async fn sync_remote(&mut self) -> Result<Option<ReloadOutcome>> {
        let Some(remote) = self.remote.clone() else {
            return Ok(None);
        };
        let Some(manifest) = fetch_remote_manifest(&remote).await? else {
            return Ok(None);
        };
        if self.last_version.as_deref() == Some(manifest.version.as_str()) {
            return Ok(None);
        }
        let mut cache = BTreeMap::new();
        let scripts = download_manifest_scripts(&remote, &manifest, &mut cache).await?;
        let outcome = self.reload_from_scripts(scripts).await?;
        self.last_version = Some(manifest.version);
        Ok(Some(outcome))
    }

    pub async fn poll_reload(&mut self) -> Result<Option<ReloadOutcome>> {
        let mut latest = None;
        if let Some(poller) = &mut self.poller {
            while let Ok(update) = poller.rx.try_recv() {
                latest = Some(update);
            }
        }
        let Some((version, scripts)) = latest else {
            return Ok(None);
        };
        let outcome = self.reload_from_scripts(scripts).await?;
        self.last_version = Some(version);
        Ok(Some(outcome))
    }

    pub fn slash(&self, name: &str, active: &[&str]) -> Option<&str> {
        self.commands
            .iter()
            .find(|(_, (_, c))| {
                c.slash.as_ref().is_some_and(|s| s.name == name) && c.when.matches(active)
            })
            .map(|(id, _)| id.as_str())
    }

    pub(super) fn shortcuts(
        &self,
        overrides: &pi_rs_p2_tui::KeybindingsConfig,
    ) -> Vec<(String, String, When)> {
        let builtins = pi_rs_p2_tui::builtin_keybindings(Default::default());
        let reserved = pi_rs_p2_tui::resolve_privileged_keys(&builtins, overrides);
        let mut entries: Vec<_> = self.commands.iter().collect();
        entries.sort_by_key(|(id, (_, command))| {
            std::cmp::Reverse((
                command.when.specificity(),
                overrides.contains_key(id.as_str()),
            ))
        });
        let mut keys = vec![];
        for (id, (_, command)) in entries {
            let binding = overrides
                .get(id)
                .or_else(|| builtins.get(id.as_str()).map(|b| &b.default_keys))
                .or(command.keys.as_ref());
            if let Some(value) = binding {
                for key in value.as_slice() {
                    if !reserved.keys.values().any(|keys| keys.contains(key)) {
                        keys.push((key.clone(), id.clone(), command.when.clone()));
                    }
                }
            }
        }
        keys
    }

    pub async fn run(&mut self, id: &str, args: &str) -> Result<()> {
        let (index, _) = self
            .commands
            .get(id)
            .ok_or_else(|| Error::Invalid(format!("Unknown TUI command {id}")))?;
        self.guests[*index]
            .object
            .call(
                "run",
                JsonValue::from_serde(json!({"id":id,"args":args})),
                &Context::background(),
            )
            .await?;
        self.active = Some(*index);
        self.guests[*index].changed.store(true, Ordering::Release);
        Ok(())
    }

    pub fn detach(&mut self) {
        self.active = None;
    }

    pub fn panel(&self, theme: &ThemeRef) -> Result<PanelUpdate> {
        let Some(guest) = self.active.map(|i| &self.guests[i]) else {
            return Ok(PanelUpdate::Unchanged);
        };
        if !guest.changed.swap(false, Ordering::AcqRel) {
            return Ok(PanelUpdate::Unchanged);
        }
        let value: Value = guest.object.read("panel")?.to_serde()?;
        let Some(panel): Option<Panel> = serde_json::from_value(value)? else {
            return Ok(PanelUpdate::Close);
        };
        let title = line_text(&panel.title);
        Ok(PanelUpdate::Show(PanelFrame::new(
            Box::new(Style(theme.clone())),
            panel.id,
            Box::new(Lines(
                panel.lines.iter().map(|line| line_text(line)).collect(),
            )),
            move || title.clone(),
        )))
    }

    pub async fn close(self) -> Result<()> {
        let host = self.host.clone();
        drop(self);
        host.close(&Context::background()).await
    }
}

async fn fetch_remote_manifest(remote: &RemoteSource) -> Result<Option<TuiManifest>> {
    let manifest_url = remote
        .base_url
        .join("/__p2_tui")
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let response = remote
        .client
        .get(manifest_url)
        .header(header::ORIGIN, &remote.origin)
        .header(header::COOKIE, &remote.cookie)
        .send()
        .await
        .map_err(|e| Error::Invalid(format!("failed to fetch TUI manifest: {e}")))?;
    if response.status().as_u16() == 404 {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(Error::Invalid(format!(
            "TUI manifest request failed with status {}",
            response.status()
        )));
    }
    let body = response
        .bytes()
        .await
        .map_err(|e| Error::Invalid(format!("failed to read TUI manifest: {e}")))?;
    let Ok(manifest) = serde_json::from_slice::<TuiManifest>(&body) else {
        return Ok(None);
    };
    Ok(Some(manifest))
}

async fn download_manifest_scripts(
    remote: &RemoteSource,
    manifest: &TuiManifest,
    cache: &mut BTreeMap<String, Vec<u8>>,
) -> Result<Vec<PluginScript>> {
    let mut scripts = Vec::with_capacity(manifest.plugins.len());
    let mut total_bytes = 0usize;
    let mut next_cache = BTreeMap::new();

    for entry in &manifest.plugins {
        if entry.bytes > MAX_TUI_BUNDLE {
            return Err(Error::Invalid(format!(
                "TUI plugin {} exceeds {} byte limit ({} bytes)",
                entry.name, MAX_TUI_BUNDLE, entry.bytes
            )));
        }
        total_bytes = total_bytes.saturating_add(entry.bytes);
        if total_bytes > MAX_TOTAL_TUI_BUNDLES {
            return Err(Error::Invalid(format!(
                "TUI plugins exceed total {} byte budget",
                MAX_TOTAL_TUI_BUNDLES
            )));
        }
        let bytes = if let Some(cached) = cache.get(&entry.digest) {
            cached.clone()
        } else {
            if !entry.url.starts_with("/__p2_tui/") {
                return Err(Error::Invalid(format!(
                    "Invalid TUI plugin asset URL for {}: {}",
                    entry.name, entry.url
                )));
            }
            let asset_url = remote
                .base_url
                .join(&entry.url)
                .map_err(|e| Error::Invalid(e.to_string()))?;
            if asset_url.origin() != remote.base_url.origin() {
                return Err(Error::Invalid(format!(
                    "TUI plugin {} asset URL leaves origin",
                    entry.name
                )));
            }
            let response = remote
                .client
                .get(asset_url)
                .header(header::ORIGIN, &remote.origin)
                .header(header::COOKIE, &remote.cookie)
                .send()
                .await
                .map_err(|e| {
                    Error::Invalid(format!("failed to fetch TUI plugin {}: {e}", entry.name))
                })?;
            if !response.status().is_success() {
                return Err(Error::Invalid(format!(
                    "TUI plugin {} download failed with status {}",
                    entry.name,
                    response.status()
                )));
            }
            let payload = response.bytes().await.map_err(|e| {
                Error::Invalid(format!("failed to read TUI plugin {}: {e}", entry.name))
            })?;
            if payload.len() > MAX_TUI_BUNDLE {
                return Err(Error::Invalid(format!(
                    "TUI plugin {} payload exceeds {} byte limit ({} bytes)",
                    entry.name,
                    MAX_TUI_BUNDLE,
                    payload.len()
                )));
            }
            let actual = digest(&payload);
            if actual != entry.digest {
                return Err(Error::Invalid(format!(
                    "TUI plugin {} digest mismatch: expected {}, got {}",
                    entry.name, entry.digest, actual
                )));
            }
            payload.to_vec()
        };
        next_cache.insert(entry.digest.clone(), bytes.clone());
        scripts.push(PluginScript::wasm(&entry.name, bytes, false));
    }

    *cache = next_cache;
    Ok(scripts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_rs_tui::Component;

    #[tokio::test]
    async fn discovered_wit_command_publishes_guest_rendered_panel_after_invocation()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        std::fs::write(
            root.path().join("notes.tui.wasm"),
            include_bytes!("../../../../pi-rs-p2-tui/tests/fixtures/panel/notes.tui.wasm"),
        )?;
        let mut plugins = Plugins::load(root.path().into())
            .await?
            .ok_or("missing guest")?;
        assert!(
            plugins
                .slash("notes", &["chat", "owner", "overlay"])
                .is_none()
        );
        let id = plugins
            .slash("notes", &["chat", "owner"])
            .ok_or("missing slash command")?
            .to_owned();
        plugins.run(&id, "revision two").await?;
        let PanelUpdate::Show(panel) = plugins.panel(&ThemeRef::new(Default::default()))? else {
            return Err("missing published panel".into());
        };
        let rows = panel.render(80);
        assert!(rows[0].contains("Guest notes revision two"));
        assert!(rows.last().is_some_and(|line| line.contains("note row 50")));
        plugins.close().await?;
        Ok(())
    }
}
