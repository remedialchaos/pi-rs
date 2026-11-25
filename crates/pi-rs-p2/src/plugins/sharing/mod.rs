//! Durable sharing grants for worker routing.

mod http;
mod server;
mod store;
mod store_data;

pub use server::PeopleUrl;

use std::sync::Arc;

use pi_rs_chord::{Result, context::Context, contracts::Contract, host::Host, json::JsonValue};
use serde_json::json;

use crate::{
    api::v1::{FacetKind, PluginRegistry, Principal, p2_contracts::lookup},
    runtime::server::server_of,
};

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    let tables = Arc::new(store::Tables::new()?);
    registry.register("sharing", FacetKind::Server, server::facet(tables.clone())?)?;
    let catalog = Arc::new(lookup("p2.catalog")?);
    registry.register_session_authorizer("sharing", move |host, principal, session, context| {
        let tables = tables.clone();
        let catalog = catalog.clone();
        Box::pin(
            async move { authorize(&host, &tables, catalog, principal, &session, &context).await },
        )
    })
}

async fn authorize(
    host: &Host,
    tables: &store::Tables,
    catalog: Arc<Contract>,
    principal: Principal,
    session: &str,
    context: &Context,
) -> Result<bool> {
    let Principal::Guest {
        id,
        sessions,
        revision,
        ..
    } = principal
    else {
        return Ok(false);
    };
    let Some(&mode) = sessions.get(session) else {
        return Ok(false);
    };
    let natives = server_of(host)?;
    if !tables
        .current_grant(natives.store(), session, &id, revision, mode)
        .await?
    {
        return Ok(false);
    }
    let held = host
        .get(catalog, JsonValue::from_serde(json!({})), context)
        .await?;
    let row = held
        .call("get", JsonValue::from_serde(json!({"id":session})), context)
        .await?
        .to_serde()?;
    Ok(row["id"] == session
        && row
            .get("archivedAt")
            .is_some_and(serde_json::Value::is_null))
}
