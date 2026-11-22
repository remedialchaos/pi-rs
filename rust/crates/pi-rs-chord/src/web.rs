//! Pure page/environment, registered-script and stylesheet protocol. A WASM adapter applies the returned effects.
use crate::{
    Error, Result,
    surfaces::{Language, ScriptEntry, ScriptSet, Theme, theme_variables},
};
use indexmap::IndexMap;
use std::collections::BTreeSet;
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PageEffect {
    SetVariable {
        name: String,
        value: String,
    },
    RemoveVariable(String),
    ColorScheme(String),
    Language {
        lang: String,
        dir: String,
    },
    FetchScript(ScriptEntry),
    RemoveScript {
        plugin: String,
        digest: String,
    },
    PrepareSheet {
        plugin: String,
        url: String,
        after: Option<String>,
        before: Option<String>,
    },
    ActivateSheet {
        plugin: String,
        url: String,
    },
    RemoveSheet {
        plugin: String,
        url: String,
    },
    AfterDrawn(Vec<(String, String)>),
    Outdated,
}
#[derive(Default)]
pub struct Page {
    variables: IndexMap<String, String>,
}
impl Page {
    pub fn theme(&mut self, theme: &Theme) -> Vec<PageEffect> {
        let next = theme_variables(theme);
        let mut effects = self
            .variables
            .keys()
            .filter(|name| !next.contains_key(*name))
            .cloned()
            .map(PageEffect::RemoveVariable)
            .collect::<Vec<_>>();
        effects.extend(next.iter().map(|(name, value)| PageEffect::SetVariable {
            name: name.clone(),
            value: value.clone(),
        }));
        effects.push(PageEffect::ColorScheme(
            if theme.mode == crate::surfaces::ThemeMode::Dark {
                "dark"
            } else {
                "light"
            }
            .into(),
        ));
        self.variables = next;
        effects
    }
    pub fn language(&self, language: &Language) -> PageEffect {
        PageEffect::Language {
            lang: language.locale.clone(),
            dir: language.dir.clone(),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeEvent {
    Online,
    Visible,
    Restored { persisted: bool },
    NetworkChanged,
    Hidden,
}
pub fn wakes_connection(event: WakeEvent) -> bool {
    matches!(
        event,
        WakeEvent::Online
            | WakeEvent::Visible
            | WakeEvent::Restored { persisted: true }
            | WakeEvent::NetworkChanged
    )
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredScript {
    pub plugin: String,
    pub digest: String,
}
pub struct PageScripts {
    pub base: String,
    pub initial: Vec<RegisteredScript>,
    waiting: BTreeSet<(String, String)>,
}
impl PageScripts {
    pub fn new(base: impl Into<String>, initial: Vec<RegisteredScript>) -> Self {
        Self {
            base: base.into(),
            initial,
            waiting: BTreeSet::new(),
        }
    }
    pub fn fetch(&mut self, entry: ScriptEntry) -> PageEffect {
        self.waiting
            .insert((entry.name.clone(), entry.digest.clone()));
        PageEffect::FetchScript(entry)
    }
    pub fn registered(
        &mut self,
        plugin: &str,
        digest: &str,
    ) -> Option<(RegisteredScript, PageEffect)> {
        if !self.waiting.remove(&(plugin.into(), digest.into())) {
            return None;
        }
        Some((
            RegisteredScript {
                plugin: plugin.into(),
                digest: digest.into(),
            },
            PageEffect::RemoveScript {
                plugin: plugin.into(),
                digest: digest.into(),
            },
        ))
    }
    pub fn loaded(&mut self, plugin: &str, digest: &str) -> Result<()> {
        if self.waiting.remove(&(plugin.into(), digest.into())) {
            return Err(Error::Invalid(format!(
                "plugin {plugin}: script did not register {digest}"
            )));
        }
        Ok(())
    }
    pub fn failed(&mut self, plugin: &str, digest: &str) -> Option<PageEffect> {
        self.waiting
            .remove(&(plugin.into(), digest.into()))
            .then(|| PageEffect::RemoveScript {
                plugin: plugin.into(),
                digest: digest.into(),
            })
    }
    pub fn update(&self, set: &ScriptSet) -> Vec<PageEffect> {
        if set.base != self.base {
            vec![PageEffect::Outdated]
        } else {
            Vec::new()
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SheetState {
    Loading,
    Loaded,
    Failed,
}
#[derive(Clone)]
struct Sheet {
    url: String,
    state: SheetState,
}
#[derive(Default)]
pub struct PageStyles {
    current: IndexMap<String, Sheet>,
    prepared: IndexMap<String, Sheet>,
}
impl PageStyles {
    pub fn present(initial: IndexMap<String, (String, SheetState)>) -> Self {
        Self {
            current: initial
                .into_iter()
                .map(|(name, (url, state))| (name, Sheet { url, state }))
                .collect(),
            prepared: IndexMap::new(),
        }
    }
    pub fn prepare(&mut self, set: &ScriptSet) -> Vec<PageEffect> {
        let mut effects = Vec::new();
        for (index, entry) in set.plugins.iter().enumerate() {
            let loaded = entry.css.as_ref().is_some_and(|css| {
                self.current
                    .get(&entry.name)
                    .is_some_and(|s| s.url == *css && s.state == SheetState::Loaded)
            });
            if entry.css.is_none() || loaded {
                if let Some(old) = self.prepared.shift_remove(&entry.name) {
                    effects.push(PageEffect::RemoveSheet {
                        plugin: entry.name.clone(),
                        url: old.url,
                    });
                }
                continue;
            }
            let Some(css) = &entry.css else {
                continue;
            };
            if self
                .prepared
                .get(&entry.name)
                .is_some_and(|s| s.url == *css && s.state != SheetState::Failed)
            {
                continue;
            }
            if let Some(old) = self.prepared.shift_remove(&entry.name) {
                effects.push(PageEffect::RemoveSheet {
                    plugin: entry.name.clone(),
                    url: old.url,
                });
            }
            let after = if self.current.contains_key(&entry.name) {
                Some(entry.name.clone())
            } else {
                set.plugins[..index]
                    .iter()
                    .rev()
                    .find(|e| {
                        self.prepared.contains_key(&e.name) || self.current.contains_key(&e.name)
                    })
                    .map(|e| e.name.clone())
            };
            let before = if after.is_none() {
                set.plugins[index + 1..]
                    .iter()
                    .find(|e| {
                        self.current.contains_key(&e.name) || self.prepared.contains_key(&e.name)
                    })
                    .map(|e| e.name.clone())
            } else {
                None
            };
            self.prepared.insert(
                entry.name.clone(),
                Sheet {
                    url: css.clone(),
                    state: SheetState::Loading,
                },
            );
            effects.push(PageEffect::PrepareSheet {
                plugin: entry.name.clone(),
                url: css.clone(),
                after,
                before,
            });
        }
        effects
    }
    pub fn loaded(&mut self, plugin: &str, url: &str, success: bool) {
        for sheets in [&mut self.current, &mut self.prepared] {
            if let Some(sheet) = sheets.get_mut(plugin).filter(|s| s.url == url) {
                sheet.state = if success {
                    SheetState::Loaded
                } else {
                    SheetState::Failed
                };
            }
        }
    }
    pub fn cancel(&mut self) -> Vec<PageEffect> {
        self.prepared
            .drain(..)
            .map(|(plugin, sheet)| PageEffect::RemoveSheet {
                plugin,
                url: sheet.url,
            })
            .collect()
    }
    pub fn settle(&mut self, set: &ScriptSet, kept: &BTreeSet<String>) -> Vec<PageEffect> {
        let mut effects = Vec::new();
        let mut retired = Vec::new();
        for entry in &set.plugins {
            let prepared = self.prepared.shift_remove(&entry.name);
            if kept.contains(&entry.name) {
                if let Some(sheet) = prepared {
                    effects.push(PageEffect::RemoveSheet {
                        plugin: entry.name.clone(),
                        url: sheet.url,
                    });
                }
                continue;
            }
            if let Some(sheet) = prepared {
                effects.push(PageEffect::ActivateSheet {
                    plugin: entry.name.clone(),
                    url: sheet.url.clone(),
                });
                if let Some(old) = self.current.insert(entry.name.clone(), sheet) {
                    retired.push((entry.name.clone(), old.url));
                }
            } else if entry.css.is_none()
                && let Some(old) = self.current.shift_remove(&entry.name)
            {
                retired.push((entry.name.clone(), old.url));
            }
        }
        effects.extend(self.cancel());
        let names = set
            .plugins
            .iter()
            .map(|p| p.name.as_str())
            .collect::<BTreeSet<_>>();
        let removed = self
            .current
            .keys()
            .filter(|name| !names.contains(name.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        for plugin in removed {
            if let Some(sheet) = self.current.shift_remove(&plugin) {
                retired.push((plugin, sheet.url));
            }
        }
        if !retired.is_empty() {
            effects.push(PageEffect::AfterDrawn(retired));
        }
        effects
    }
    pub fn current(&self, plugin: &str) -> Option<&str> {
        self.current.get(plugin).map(|s| s.url.as_str())
    }
}
