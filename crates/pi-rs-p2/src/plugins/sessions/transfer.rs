use std::sync::Arc;

use pi_rs_chord::{
    Error, Result,
    context::Context,
    data::ChordError,
    host::{Facet, Members, ServeOptions, define_facet},
};
use pi_rs_durable::{
    EntryDraft, EntryHead, EntryQuery, ROOT_CONVERSATION_ID, harness::Harness,
    session::CommitOptions,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::v1::{
    Principal,
    node::{host_files, session_directory},
    p2_contracts::lookup,
    worker::worker_of,
};

use super::{arg, copy, wire};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct History {
    conversation: u64,
    before: u64,
}

pub(super) fn facet() -> Result<Facet> {
    let contract = Arc::new(lookup("p2.session-transfer")?);
    Ok(define_facet(move |host| {
        let worker = worker_of(&host.host())?;
        let id = worker.init.session()?.id;
        host.serve(contract.clone(), move |params, principal, _| {
            if principal.cloned().map(serde_json::from_value).transpose()? != Some(Principal::Server) {
                return Err(ChordError::new("forbidden", "server only").into());
            }
            if params.to_serde()?["session"] != id {
                return Err(Error::Invalid("Session transfer address mismatch".into()));
            }
            let mut members = Members::variant("server");
            for method in ["history", "copyFiles", "seed"] {
                let worker = worker.clone();
                members.method(method, move |args, ctx| {
                    let worker = worker.clone();
                    Box::pin(async move {
                        let args: Value = args.to_serde()?;
                        worker.call(async {
                            let harness = worker.harness()?;
                            match method {
                                "history" => {
                                    Ok(wire(json!({"entries": history(&harness.durable, serde_json::from_value(args)?, &ctx.context).await?})))
                                }
                                "copyFiles" => {
                                    let id = worker.init.session()?.id;
                                    let to = arg(&args, "to")?;
                                    if to == id {
                                        return Err(Error::Invalid("Cannot copy files to the same session".into()));
                                    }
                                    let directory = &worker.init.data_dir;
                                    copy::copy(
                                        &host_files(&session_directory(directory, &id)?),
                                        &host_files(&session_directory(directory, to)?),
                                    ).await?;
                                    Ok(wire(Value::Null))
                                }
                                _ => {
                                    seed(&harness.durable, serde_json::from_value(args["entries"].clone())?, &ctx.context).await?;
                                    Ok(wire(Value::Null))
                                }
                            }
                        }).await
                    })
                })?;
            }
            Ok(members)
        }, ServeOptions::default())
    }))
}

async fn history(
    harness: &Harness,
    request: History,
    context: &Context,
) -> Result<Vec<EntryDraft>> {
    let found = harness
        .conversation(request.conversation, context)
        .await
        .map_err(durable_error)?
        .ok_or_else(|| Error::Invalid(format!("Unknown conversation {}", request.conversation)))?;
    let top = found
        .commit(
            async |tx| {
                Ok(tx
                    .conversation(request.conversation)
                    .await?
                    .is_some_and(|row| row.owner.is_none()))
            },
            context,
            CommitOptions::default(),
        )
        .await
        .map_err(durable_error)?;
    if !top {
        return Err(Error::Invalid(
            "Forking a subagent conversation is not supported yet".into(),
        ));
    }
    let entries = found
        .entries(
            EntryQuery {
                max_entry_id: Some(request.before),
                ..Default::default()
            },
            2,
            None,
            context,
        )
        .await
        .map_err(durable_error)?;
    let mut entries = entries.items.into_iter();
    if entries
        .next()
        .is_none_or(|entry| entry.id != request.before)
    {
        return Err(Error::Invalid("Unknown message".into()));
    }
    let Some(previous) = entries.next() else {
        return Ok(Vec::new());
    };
    let view = found
        .context(Some(previous.id), context)
        .await
        .map_err(durable_error)?;
    Ok(view
        .entries
        .into_iter()
        .zip(view.contributions)
        .enumerate()
        .map(|(index, (entry, model))| EntryDraft {
            kind: entry.kind,
            data: entry.data,
            model: (!model.is_empty()).then_some(model),
            head: (index == 0 && view.head.is_some()).then_some(EntryHead::SelfEntry),
            edits: None,
        })
        .collect())
}

async fn seed(harness: &Harness, entries: Vec<EntryDraft>, context: &Context) -> Result<()> {
    let root = harness
        .conversation(ROOT_CONVERSATION_ID, context)
        .await
        .map_err(durable_error)?
        .ok_or_else(|| Error::Invalid("No main conversation".into()))?;
    root.commit(
        async |tx| {
            if tx.latest_entry_id(ROOT_CONVERSATION_ID).await?.is_some() {
                return Err(pi_rs_durable::session::Error::invalid(
                    "The session has history already",
                ));
            }
            for entry in entries {
                tx.append_entry(ROOT_CONVERSATION_ID, entry).await?;
            }
            Ok(())
        },
        context,
        CommitOptions::default(),
    )
    .await
    .map_err(durable_error)
}

fn durable_error(error: impl std::fmt::Display) -> Error {
    Error::Invalid(error.to_string())
}
