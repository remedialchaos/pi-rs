//! Project a local WIT facet's command metadata and panel state onto the terminal.
use super::when::When;
use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    context::Context,
    host::{Held, Host, HostOptions, create_host},
    json::JsonValue,
    node::plugins::{PluginRoot, PluginRuntime, PluginScript, discover_plugins},
    state::Subscription,
};
use pi_rs_p2_tui::{
    CORE_COMMANDS, RenderContext, ThemeRef,
    components::{PanelContent, PanelFrame},
    line_text,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Deserialize)]
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
#[derive(Deserialize)]
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
pub(super) enum PanelUpdate {
    Unchanged,
    Close,
    Show(PanelFrame),
}
struct Guest {
    object: Held,
    changed: Arc<AtomicBool>,
    _subscription: Subscription,
}
impl Drop for Guest {
    fn drop(&mut self) {
        self.object.release();
    }
}
pub(super) struct Plugins {
    host: Host,
    _runtime: PluginRuntime,
    commands: IndexMap<String, (usize, Command)>,
    guests: Vec<Guest>,
    active: Option<usize>,
}

impl Plugins {
    pub(super) async fn load(root: PathBuf) -> Result<Option<Self>> {
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
        let ctx = Context::background();
        let host = create_host(HostOptions::default())?;
        let (runtime, report) = PluginRuntime::load(host.clone(), vec![], scripts, &ctx).await?;
        if let Some(failed) = report.failed.first() {
            return Err(Error::Invalid(format!(
                "TUI facet {}: {}",
                failed.plugin, failed.error
            )));
        }
        let mut plugins = Self {
            host,
            _runtime: runtime,
            commands: IndexMap::new(),
            guests: Vec::new(),
            active: None,
        };
        for plugin in discovery.plugins {
            if !plugin.facets.contains_key("tui") {
                continue;
            }
            let Some(contract) = plugins.host.served_contract(&plugin.name) else {
                continue;
            };
            let object = plugins
                .host
                .get(contract, JsonValue::Object(Default::default()), &ctx)
                .await?;
            let commands: IndexMap<String, Command> =
                serde_json::from_value(object.read("commands")?.to_serde()?)?;
            for (id, mut command) in commands {
                if id.starts_with("tui.")
                    || pi_rs_p2_tui::is_privileged(&id)
                    || CORE_COMMANDS.iter().any(|c| c.id == id)
                    || plugins.commands.contains_key(&id)
                {
                    continue;
                }
                if let Some(slash) = &command.slash
                    && (CORE_COMMANDS.iter().any(|c| c.slash == Some(&slash.name))
                        || plugins
                            .commands
                            .values()
                            .any(|(_, c)| c.slash.as_ref().is_some_and(|s| s.name == slash.name)))
                {
                    command.slash = None;
                }
                plugins.commands.insert(id, (plugins.guests.len(), command));
            }
            let changed = Arc::new(AtomicBool::new(false));
            let mark = changed.clone();
            let subscription = object.member("panel")?.subscribe(
                move |_, _| {
                    mark.store(true, Ordering::Release);
                    Ok(())
                },
                |_| {},
            );
            plugins.guests.push(Guest {
                object,
                changed,
                _subscription: subscription,
            });
        }
        Ok(Some(plugins))
    }
    pub(super) fn slash(&self, name: &str, active: &[&str]) -> Option<&str> {
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
    pub(super) async fn run(&mut self, id: &str, args: &str) -> Result<()> {
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
    pub(super) fn detach(&mut self) {
        self.active = None;
    }
    pub(super) fn panel(&self, theme: &ThemeRef) -> Result<PanelUpdate> {
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
    pub(super) async fn close(self) -> Result<()> {
        let host = self.host.clone();
        drop(self);
        host.close(&Context::background()).await
    }
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
