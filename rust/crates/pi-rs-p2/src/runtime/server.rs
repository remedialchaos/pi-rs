use std::sync::Arc;

use indexmap::IndexMap;
use pi_rs_chord::{
    Result,
    contracts::{Contract, Member, schema as s},
    host::{Facet, Members, ServeOptions, define_facet},
    json::JsonValue,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub app: String,
    pub data_dir: String,
    pub fake: bool,
    pub allowed_hosts: Vec<String>,
    pub port: u16,
}

pub fn server_contract() -> Result<Contract> {
    Contract::local(
        "p2.server",
        2,
        vec![],
        IndexMap::from([(
            "info".into(),
            Member::State(s::obj([
                ("app", s::required(s::str())),
                ("dataDir", s::required(s::str())),
                ("fake", s::required(s::bool())),
                ("allowedHosts", s::required(s::arr(s::str()))),
                ("port", s::required(s::num())),
            ])),
        )]),
    )
}

pub(crate) fn info_facet(info: ServerInfo) -> Result<Facet> {
    let contract = Arc::new(server_contract()?);
    let info = JsonValue::from_serde(serde_json::to_value(info)?);
    Ok(define_facet(move |host| {
        let info = info.clone();
        host.serve(
            contract.clone(),
            move |_, _, object| {
                let mut members = Members::new();
                members.state("info", object.writable(info.clone())?)?;
                Ok(members)
            },
            ServeOptions::default(),
        )
    }))
}
