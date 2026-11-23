use std::sync::Arc;

use indexmap::IndexMap;
use pi_rs_chord::{
    Result,
    contracts::{Contract, Member, schema as s},
    data::ChordError,
    host::{Facet, Members, ObjectSelf, ServeOptions, define_facet},
    json::JsonValue,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::principals::{GrantMode, Principal};
use crate::api::v1::p2_contracts::lookup;

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
    let access = Arc::new(lookup("p2.access")?);
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
        )?;
        host.serve(
            access.clone(),
            |_, principal, object| access_members(principal, object),
            ServeOptions::default(),
        )
    }))
}

fn access_members(principal: Option<&serde_json::Value>, object: &ObjectSelf) -> Result<Members> {
    match principal.and_then(|value| Principal::deserialize(value).ok()) {
        Some(Principal::Owner { .. }) => Ok(Members::variant("owner")),
        Some(Principal::Guest {
            id,
            name,
            email,
            home,
            sessions,
            ..
        }) => {
            let mode = sessions.get(&home).copied().unwrap_or(GrantMode::View);
            let mut grant = json!({
                "id": id, "name": name, "session": home, "mode": mode, "sessions": sessions
            });
            if let Some(email) = email {
                grant["email"] = email.into();
            }
            let mut members = Members::variant("guest");
            members.state("grant", object.writable(JsonValue::from_serde(grant))?)?;
            Ok(members)
        }
        _ => Err(ChordError::new("forbidden", "owners and guests only").into()),
    }
}
