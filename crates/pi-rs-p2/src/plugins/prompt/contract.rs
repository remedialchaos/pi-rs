//! Prompt resources contracts and `PluginSkills` cross-plugin service.

use std::sync::{Arc, Mutex};

use indexmap::IndexMap;
use pi_rs_chord::{
    Result,
    contracts::{Contract, Member, schema as s},
    host::Host,
};
use serde::{Deserialize, Serialize};

use crate::api::v1::{
    p2_contracts::lookup,
    server::{install_service, service_of},
};

pub const PLUGIN_SKILLS_SERVICE: &str = "p2.plugin-skills";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TemplateSource {
    Global,
    Project,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateInfo {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    pub source: TemplateSource,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
}

#[derive(Default)]
pub struct PluginSkillsService {
    dirs: Mutex<IndexMap<String, String>>,
}

impl PluginSkillsService {
    pub fn contribute(&self, plugin: impl Into<String>, dir: impl Into<String>) {
        self.dirs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(plugin.into(), dir.into());
    }

    pub fn remove(&self, plugin: &str) {
        self.dirs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shift_remove(plugin);
    }

    pub fn dirs(&self) -> Vec<String> {
        let mut list: Vec<String> = self
            .dirs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        list.sort();
        list
    }
}

pub fn get_or_install_plugin_skills(host: &Host) -> Result<Arc<PluginSkillsService>> {
    match service_of::<PluginSkillsService>(host, PLUGIN_SKILLS_SERVICE) {
        Ok(existing) => Ok(existing),
        Err(_) => {
            let service = Arc::new(PluginSkillsService::default());
            install_service(host, PLUGIN_SKILLS_SERVICE, &service)?;
            Ok(service)
        }
    }
}

pub fn prompt_local_contract() -> Result<Arc<Contract>> {
    let contract = Contract::local(
        "p2.prompt",
        4,
        vec![],
        IndexMap::from([(
            "expand".into(),
            Member::Method {
                args: s::obj([
                    ("text", s::required(s::str())),
                    ("conversation", s::required(s::num())),
                ]),
                result: s::obj([
                    ("text", s::required(s::str())),
                    ("skill", s::opt(s::obj([("name", s::required(s::str()))]))),
                ]),
            },
        )]),
    )?;
    Ok(Arc::new(contract))
}

pub fn prompt_resources_contract() -> Result<Arc<Contract>> {
    Ok(Arc::new(lookup("p2.prompt-resources")?))
}
