use crate::{
    Error, Result,
    brain::{Brain, Organization, Route, Status},
    radius_account::{RadiusAccount, string_at},
    server_config::ServerConfig,
};
use serde::Deserialize;
use std::{collections::HashMap, fs, path::Path, time::Duration};
use unicode_segmentation::UnicodeSegmentation;

pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Default, Deserialize)]
struct List {
    servers: Vec<Entry>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    server_id: String,
    name: Option<String>,
    organization: Organization,
    status: Status,
    shared: Option<bool>,
}

pub struct BrainDirectory {
    list: List,
    pub brains: Vec<Brain>,
    fixture: bool,
    enabled: bool,
    pub shows_tabs: bool,
    signed_in: bool,
}
impl BrainDirectory {
    pub fn new(fixture: Option<&Path>) -> Self {
        let list = fixture
            .and_then(|path| fs::File::open(path).ok())
            .and_then(|file| serde_json::from_reader(file).ok());
        Self {
            fixture: list.is_some(),
            list: list.unwrap_or_default(),
            enabled: true,
            signed_in: false,
            brains: Vec::new(),
            shows_tabs: false,
        }
    }
    pub fn is_fixture(&self) -> bool {
        self.fixture
    }
    pub fn brain(&self, id: &str) -> Option<&Brain> {
        self.brains.iter().find(|b| b.id == id)
    }
    pub fn local(&self) -> Option<&Brain> {
        self.brains.iter().find(|b| b.is_direct())
    }
    pub fn clear_account(&mut self) {
        if !self.fixture {
            self.signed_in = false;
            self.enabled = true;
            self.list = List::default();
        }
    }

    pub async fn poll(
        &mut self,
        account: &RadiusAccount,
        config: &ServerConfig,
        running: bool,
    ) -> Result<()> {
        let result = if self.fixture {
            Ok(())
        } else if !account.signed_in() {
            self.clear_account();
            Ok(())
        } else {
            self.signed_in = true;
            match account.servers().await {
                Ok(data) => match serde_json::from_value(data) {
                    Ok(list) => {
                        self.list = list;
                        self.enabled = true;
                        Ok(())
                    }
                    Err(e) => Err(e.into()),
                },
                Err(Error::NotEnabled) => {
                    self.enabled = false;
                    Ok(())
                }
                Err(e) => {
                    if !account.signed_in() {
                        self.clear_account();
                    }
                    Err(e)
                }
            }
        };
        self.rebuild(config, running);
        result
    }
    pub fn rebuild(&mut self, config: &ServerConfig, running: bool) {
        let local_id = string_at(&config.data_dir.join("radius.json"), "serverId")
            .unwrap_or_else(|| format!("local-{}", config.port));
        let status = if running {
            Status::Online
        } else {
            Status::Offline
        };
        let direct = Route::Direct {
            origin: config.origin(),
            data_dir: config.data_dir.clone(),
        };
        let mut groups: HashMap<&str, (&Organization, Vec<&Entry>)> = HashMap::new();
        for entry in &self.list.servers {
            groups
                .entry(&entry.organization.id)
                .or_insert_with(|| (&entry.organization, Vec::new()))
                .1
                .push(entry);
        }
        let mut groups: Vec<_> = groups.into_values().collect();
        groups.sort_unstable_by(|(a, _), (b, _)| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
        let mut brains = Vec::with_capacity(self.list.servers.len() + 1);
        let mut matched = false;
        for (org, mut entries) in groups {
            entries.sort_by(|a, b| a.server_id.cmp(&b.server_id));
            for entry in entries {
                let local = entry.server_id == local_id;
                matched |= local;
                let route = if local {
                    direct.clone()
                } else {
                    Route::Remote {
                        organization: org.id.clone(),
                        server: entry.server_id.clone(),
                    }
                };
                let name = entry
                    .name
                    .clone()
                    .unwrap_or_else(|| entry.server_id.graphemes(true).take(8).collect());
                brains.push(Brain {
                    id: entry.server_id.clone(),
                    name,
                    organization: Some(org.clone()),
                    status: if local { status } else { entry.status },
                    shared: !local && entry.shared.unwrap_or(false),
                    route,
                });
            }
        }
        if !matched {
            let name = string_at(&config.data_dir.join("identity.json"), "name")
                .unwrap_or_else(|| "This Mac".into());
            brains.insert(
                0,
                Brain {
                    id: local_id,
                    name,
                    organization: None,
                    shared: false,
                    status,
                    route: direct,
                },
            );
        }
        self.brains = brains;
        self.shows_tabs = self.fixture || (self.signed_in && self.enabled);
    }
    pub fn tab_labels(&self) -> impl Iterator<Item = std::borrow::Cow<'_, str>> {
        let first = self.brains.iter().find_map(|b| b.organization.as_ref());
        let grouped = first.is_some_and(|org| {
            self.brains
                .iter()
                .any(|b| b.organization.as_ref().is_some_and(|b| b.id != org.id))
        });
        self.brains.iter().map(move |brain| {
            if grouped && let Some(org) = &brain.organization {
                return format!("{} / {}", org.name, brain.label()).into();
            }
            brain.label()
        })
    }
}
