//! `dev-tools` built-in plugin: exposes `p2.plugin-reload` to session workers.

use std::sync::Arc;

use pi_rs_chord::{
    Result,
    data::ChordError,
    host::{Members, ServeOptions, define_facet},
    json::JsonValue,
};
use serde::Deserialize;
use serde_json::json;

use crate::api::v1::{
    FacetKind, PluginRegistry, Principal, p2_contracts::lookup, server::service_of,
};

/// Native reload callback service key registered by the development host when reload is available.
pub const PLUGIN_RELOAD_SERVICE: &str = "p2.plugin-reload-handler";

pub type ReloadHandler =
    dyn Fn(Option<Vec<String>>) -> pi_rs_chord::sync::SyncFuture<JsonValue> + Send + Sync;

#[derive(Deserialize)]
struct ReloadArgs {
    plugins: Option<Vec<String>>,
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    let contract = Arc::new(lookup("p2.plugin-reload")?);
    registry.register(
        "dev-tools",
        FacetKind::Server,
        define_facet(move |host| {
            host.serve(
                contract.clone(),
                move |_, principal, object| {
                    let parsed = principal
                        .and_then(|v| Principal::deserialize(v).ok())
                        .ok_or_else(|| ChordError::new("forbidden", "session workers only"))?;
                    if !matches!(parsed, Principal::Worker { .. }) {
                        return Err(ChordError::new("forbidden", "session workers only").into());
                    }
                    let host_handle = object.host();
                    let mut members = Members::variant("worker");
                    members.method("reload", move |args, _| {
                        let host_handle = host_handle.clone();
                        Box::pin(async move {
                            let parsed: ReloadArgs = serde_json::from_value(args.to_serde()?)?;
                            match service_of::<Arc<ReloadHandler>>(
                                &host_handle,
                                PLUGIN_RELOAD_SERVICE,
                            ) {
                                Ok(handler) => handler(parsed.plugins).await,
                                Err(_) => Ok(JsonValue::from_serde(json!({
                                    "refused": "plugin reload is not active on this server",
                                    "plugins": [],
                                    "logs": []
                                }))),
                            }
                        })
                    })?;
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    )
}
