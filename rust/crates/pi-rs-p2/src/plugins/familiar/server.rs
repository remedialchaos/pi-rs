//! The Familiar's server side: persisted session ID (`p2.familiar-store`) and owner view (`p2.familiar`).

use std::sync::Arc;

use pi_rs_chord::{
    Error, Result,
    data::ChordError,
    documents::Definition,
    host::{Facet, Members, ServeOptions, define_facet},
    json::JsonValue,
    watch::WatchCall,
};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::api::v1::{Principal, p2_contracts::lookup};

use super::FAMILIAR_TITLE;

fn chosen_definition() -> Result<Arc<Definition>> {
    let schema = JsonValue::from_serde(json!({
        "type": "object",
        "properties": {
            "session": { "anyOf": [{ "type": "string" }, { "type": "null" }] }
        },
        "required": ["session"],
        "additionalProperties": false
    }));
    let def = Definition::new("p2.familiar", schema, 1, || {
        Ok(JsonValue::from_serde(json!({ "session": null })))
    })?
    .persisted(true, |_, _, _| true);
    Ok(Arc::new(def))
}

fn home_dir() -> String {
    std::env::var("HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/".to_owned())
}

pub fn server_facet() -> Facet {
    define_facet(|host| {
        let store_contract = Arc::new(lookup("p2.familiar-store")?);
        let familiar_contract = Arc::new(lookup("p2.familiar")?);
        let catalog_contract = Arc::new(lookup("p2.catalog")?);
        let ops_contract = Arc::new(lookup("p2.session-operations")?);
        let chosen_def = chosen_definition()?;

        host.serve(
            store_contract.clone(),
            move |_, _, object| {
                let chosen = object.state(chosen_def.clone())?;
                let catalog = object.get(
                    catalog_contract.clone(),
                    JsonValue::Object(Default::default()),
                )?;
                let operations =
                    object.get(ops_contract.clone(), JsonValue::Object(Default::default()))?;

                let chosen_source = chosen.source()?;
                let session_state = object.follow(move |target, ctx| {
                    let chosen_source = chosen_source.clone();
                    Box::pin(async move {
                        let initial = chosen_source.value()?.to_json().to_serde()?;
                        target.set(
                            JsonValue::from_serde(initial["session"].clone()),
                            &ctx.context,
                        )?;
                        let changed = target.clone();
                        let ended = target.clone();
                        let subscription = chosen_source.subscribe(
                            move |call, context| match call {
                                WatchCall::Value { value, .. } => {
                                    let serde_val = value.to_serde()?;
                                    changed.set(
                                        JsonValue::from_serde(serde_val["session"].clone()),
                                        context,
                                    )
                                }
                                WatchCall::Failed { error } => changed.fail(error, context),
                            },
                            move |_| ended.detach("ended"),
                        );
                        Ok(Box::new(move || drop(subscription)) as Box<dyn FnOnce() + Send>)
                    })
                })?;

                let open_lock = Arc::new(Mutex::new(()));
                let mut members = Members::new();
                members.state("session", session_state)?;
                members.method("open", move |_, ctx| {
                    let chosen = chosen.clone();
                    let catalog = catalog.clone();
                    let operations = operations.clone();
                    let open_lock = open_lock.clone();
                    Box::pin(async move {
                        let _guard = open_lock.lock().await;
                        let current = chosen.value()?.to_serde()?;
                        let id = match current.get("session").and_then(Value::as_str) {
                            Some(id) => id.to_owned(),
                            None => {
                                let fresh = Uuid::new_v4().to_string();
                                let id_clone = fresh.clone();
                                chosen
                                    .change(&ctx.context, move |draft| {
                                        Box::pin(async move {
                                            draft.set(
                                                &[pi_rs_chord::delta::Segment::Key(
                                                    "session".into(),
                                                )],
                                                JsonValue::String(id_clone.into()),
                                            )?;
                                            Ok(())
                                        })
                                    })
                                    .await?;
                                fresh
                            }
                        };

                        catalog.wait(&ctx.context).await?;
                        operations.wait(&ctx.context).await?;
                        let sessions_val =
                            catalog.member("sessions")?.value()?.to_json().to_serde()?;
                        let row = sessions_val
                            .as_array()
                            .and_then(|arr| {
                                arr.iter().find(|s| s["id"].as_str() == Some(id.as_str()))
                            })
                            .cloned();

                        match row {
                            None => {
                                operations
                                    .call(
                                        "create",
                                        JsonValue::from_serde(json!({
                                            "id": id,
                                            "cwd": home_dir(),
                                            "title": FAMILIAR_TITLE
                                        })),
                                        &ctx.context,
                                    )
                                    .await?;
                            }
                            Some(row) if !row["archivedAt"].is_null() => {
                                operations
                                    .call(
                                        "restore",
                                        JsonValue::from_serde(json!({ "id": id })),
                                        &ctx.context,
                                    )
                                    .await?;
                            }
                            Some(_) => {}
                        }
                        Ok(JsonValue::from_serde(json!({ "id": id })))
                    })
                })?;
                Ok(members)
            },
            ServeOptions::default(),
        )?;

        host.serve(
            familiar_contract,
            move |_, principal, object| {
                let parsed: Option<Principal> = principal
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|_| {
                        Error::from(ChordError::new("forbidden", "the Familiar is the owner's"))
                    })?;
                if !parsed.is_some_and(|p| p.is_owner()) {
                    return Err(ChordError::new("forbidden", "the Familiar is the owner's").into());
                }
                let store = object.get(
                    store_contract.clone(),
                    JsonValue::Object(Default::default()),
                )?;
                let follow_store = store.clone();
                let session_state = object.follow(move |target, ctx| {
                    let store = follow_store.clone();
                    Box::pin(async move {
                        store.wait(&ctx.context).await?;
                        let member = store.member("session")?;
                        target.set(member.value()?.to_json(), &ctx.context)?;
                        let changed = target.clone();
                        let ended = target.clone();
                        let subscription = member.subscribe(
                            move |call, context| match call {
                                WatchCall::Value { value, .. } => changed.set(value, context),
                                WatchCall::Failed { error } => changed.fail(error, context),
                            },
                            move |_| ended.detach("ended"),
                        );
                        Ok(Box::new(move || drop(subscription)) as Box<dyn FnOnce() + Send>)
                    })
                })?;

                let mut members = Members::variant("owner");
                members.state("session", session_state)?;
                members.method("open", move |_, ctx| {
                    let store = store.clone();
                    Box::pin(async move {
                        store.wait(&ctx.context).await?;
                        store.call("open", JsonValue::Null, &ctx.context).await
                    })
                })?;
                Ok(members)
            },
            ServeOptions::default(),
        )?;

        Ok(())
    })
}
