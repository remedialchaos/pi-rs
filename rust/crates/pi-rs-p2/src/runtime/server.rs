#[path = "server_services.rs"]
mod services;
pub use services::*;

use std::sync::Arc;

use pi_rs_chord::{
    Error, Result,
    contracts::Contract,
    data::ChordError,
    host::{Facet, Members, ObjectSelf, ServeOptions, WeakHost, define_facet},
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
    lookup("p2.server")
}

pub(crate) fn info_facet(natives: &Arc<ServerNatives>, server: WeakHost) -> Result<Facet> {
    let contract = Arc::new(server_contract()?);
    let access = Arc::new(lookup("p2.access")?);
    let hosts = Arc::new(hosts_contract()?);
    let names = super::server_name::ServerNames::new(natives.info.data_dir.clone())?;
    let info = JsonValue::from_serde(serde_json::to_value(&natives.info)?);
    let natives = Arc::downgrade(natives);
    Ok(define_facet(move |host| {
        names.register(host)?;
        serve_registries(host)?;
        let natives = natives.clone();
        host.serve(
            hosts.clone(),
            move |_, _, object| {
                let mut members = Members::new();
                let mut states = natives
                    .upgrade()
                    .ok_or(pi_rs_chord::Error::Closed)?
                    .workers
                    .observe();
                members.state(
                    "workers",
                    object.follow(move |target, ctx| {
                        Box::pin(async move {
                            let snapshot = serde_json::to_value(&*states.borrow_and_update())?;
                            target.set(JsonValue::from_serde(snapshot), &ctx.context)?;
                            let task = tokio::spawn(async move {
                                let context = pi_rs_chord::context::Context::background();
                                while states.changed().await.is_ok() {
                                    let update =
                                        serde_json::to_value(&*states.borrow_and_update())?;
                                    target.set(JsonValue::from_serde(update), &context)?;
                                }
                                Ok::<_, pi_rs_chord::Error>(())
                            });
                            Ok(Box::new(move || task.abort()) as Box<dyn FnOnce() + Send>)
                        })
                    })?,
                )?;
                let natives = natives.clone();
                members.method("stop", move |args, _| {
                    let natives = natives.clone();
                    Box::pin(async move {
                        let args = args.to_serde()?;
                        let session = args["session"].as_str().ok_or_else(|| {
                            pi_rs_chord::Error::Invalid("missing session id".into())
                        })?;
                        natives
                            .upgrade()
                            .ok_or(pi_rs_chord::Error::Closed)?
                            .workers
                            .stop(session)
                            .await?;
                        Ok(JsonValue::Null)
                    })
                })?;
                Ok(members)
            },
            ServeOptions::default(),
        )?;
        let info = info.clone();
        let server = server.clone();
        host.serve(
            contract.clone(),
            move |_, _, object| {
                let mut members = Members::new();
                members.state("info", object.writable(info.clone())?)?;
                let owners = server.clone();
                members.method("revokeOwners", move |args, ctx| {
                    let server = owners.upgrade();
                    Box::pin(async move {
                        let server = server.ok_or(Error::Closed)?;
                        if let Some(device) = args.to_serde()?.get("device") {
                            server
                                .revoke(&json!({"device": device}), &ctx.context)
                                .await?;
                        } else {
                            for group in ["remote", "local"] {
                                server
                                    .revoke(&json!({"owners": group}), &ctx.context)
                                    .await?;
                            }
                        }
                        Ok(JsonValue::Null)
                    })
                })?;
                let guests = server.clone();
                members.method("revokeGuests", move |args, ctx| {
                    let server = guests.upgrade();
                    Box::pin(async move {
                        let server = server.ok_or(Error::Closed)?;
                        let id = args.to_serde()?["id"].clone();
                        server.revoke(&json!({"guest": id}), &ctx.context).await?;
                        Ok(JsonValue::Null)
                    })
                })?;
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

pub fn hosts_contract() -> Result<Contract> {
    lookup("p2.hosts")
}
