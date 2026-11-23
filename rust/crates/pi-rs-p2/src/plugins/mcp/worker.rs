//! A native worker keeps the server's view held for its lifetime and publishes a Durable extension.

use std::{sync::Arc, time::Duration};

use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::Contract,
    host::{Facet, Held, Members, RefState, ServeOptions, define_facet},
    watch::NativeWatchCall,
};
use pi_rs_durable::{
    CommitChange, DocDefinition, DocToken, DocumentCommitChange, ROOT_CONVERSATION_ID,
    documents::{DocumentSemantics, InitialDocument, LatestFork, resolve_address},
    env::FileErrorCode,
};
use serde_json::{Value, json};

use crate::api::v1::{
    p2_contracts::lookup,
    worker::{WorkerProcess, worker_of},
};
use crate::runtime::location::{Location, same_location};

use super::tools::{NAME, Offer, extension, locate};

const PROJECT_LIMIT: usize = 256 * 1024;
const SUBAGENTS: &str = "p2.subagents";

pub(super) fn facet() -> Result<Facet> {
    let view = Arc::new(lookup("p2.mcp")?);
    let local = Arc::new(Contract::local(
        "p2.mcp-worker",
        1,
        Vec::new(),
        Default::default(),
    )?);
    Ok(define_facet(move |host| {
        let view = view.clone();
        host.serve(
            local.clone(),
            move |_, _, object| {
                let weak = object.host().downgrade();
                let view = view.clone();
                object.on_live(move |ctx| {
                    let (weak, view) = (weak.clone(), view.clone());
                    Box::pin(async move {
                        let host = weak.upgrade().ok_or(Error::Closed)?;
                        let worker = worker_of(&host)?;
                        if worker.init.session().is_err() {
                            return Ok(());
                        }
                        // Begin after Chord pre-ready opens the durable session. Ready also lets the
                        // parent publish the worker generation on which the server's call depends.
                        let ready = async {
                            loop {
                                if let Ok(parts) = worker.harness() {
                                    return parts;
                                }
                                tokio::time::sleep(Duration::from_millis(25)).await;
                            }
                        };
                        let parts = ctx.context.await_with(ready).await.map_err(invalid)?;
                        let held = host.hold(view, json!({}).into(), &ctx.context)?;
                        let run = async {
                            tokio::try_join!(
                                leases(&worker, &held, &parts.durable.session, &ctx.context),
                                offer_tools(&worker, &held, &ctx.context),
                            )
                            .map(|_| ())
                        };
                        let result = ctx.context.await_with(run).await;
                        held.release();
                        worker.registries.extensions.uninstall(NAME)?;
                        match result {
                            Ok(result) => result,
                            Err(_) => Ok(()),
                        }
                    })
                });
                Ok(Members::new())
            },
            ServeOptions::default(),
        )
    }))
}

async fn leases(
    worker: &WorkerProcess,
    held: &Held,
    session: &pi_rs_durable::Session,
    ctx: &Context,
) -> Result<()> {
    let (commit, mut commits) = tokio::sync::watch::channel(0u64);
    let _commit = session.follow_commits(move |publication, _| {
        let changes = match publication {
            pi_rs_durable::session::ObservedCommit::Stored(commit) => &commit.changes,
            pi_rs_durable::session::ObservedCommit::Memory(changes) => changes,
        };
        if changes.iter().any(|change| matches!(
            change,
            CommitChange::Document(DocumentCommitChange::Change { record, .. } | DocumentCommitChange::Retire { record, .. })
                if record.kind == SUBAGENTS
        )) {
            commit.send_modify(|value| *value = value.wrapping_add(1));
        }
        Ok(())
    }).map_err(invalid)?;
    let mut moves = worker.location.subscribe();
    let state = held.state();
    let (activate, mut activations) = tokio::sync::watch::channel(0u64);
    let _state = state.observe(
        ctx,
        move |event, _| {
            if matches!(
                event,
                NativeWatchCall::Value {
                    value: RefState::Live
                }
            ) {
                activate.send_modify(|value| *value = value.wrapping_add(1));
            }
            Ok(())
        },
        |_| {},
    )?;
    loop {
        commits.borrow_and_update();
        activations.borrow_and_update();
        while moves.try_recv().is_ok() {}
        let sent = async {
            held.wait(ctx).await?;
            let locations = work_locations(worker, session, ctx).await?;
            let projects = project_files(worker, &locations, ctx).await?;
            held.call(
                "setLocations",
                json!({"locations":locations,"projects":projects}).into(),
                ctx,
            )
            .await
        }
        .await;
        match sent {
            Ok(_) => {
                tokio::select! {
                    _ = commits.changed() => {},
                    _ = activations.changed() => {},
                    _ = moves.recv() => {},
                }
            }
            Err(Error::Chord(error)) if error.code == "forbidden" => return Ok(()),
            Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
}

async fn offer_tools(worker: &Arc<WorkerProcess>, held: &Held, ctx: &Context) -> Result<()> {
    let servers = held.member("servers")?;
    let leases = held.member("leases")?;
    let (changed, mut changes) = tokio::sync::watch::channel(0u64);
    let updated = changed.clone();
    let _servers = servers.subscribe(
        move |_, _| {
            updated.send_modify(|value| *value = value.wrapping_add(1));
            Ok(())
        },
        |_| {},
    );
    let _leases = leases.subscribe(
        move |_, _| {
            changed.send_modify(|value| *value = value.wrapping_add(1));
            Ok(())
        },
        |_| {},
    );
    let mut settings = worker.registries.settings.subscribe();
    let mut moves = worker.location.subscribe();
    let mut previous = Value::Null;
    loop {
        changes.borrow_and_update();
        settings.borrow_and_update();
        while moves.try_recv().is_ok() {}
        held.wait(ctx).await?;
        let mut listed: Value = servers.value()?.to_json().to_serde()?;
        let locations = leases.value()?.to_json().to_serde()?;
        let session = worker.location.value();
        let codemode = worker
            .registries
            .extensions
            .snapshot()
            .extension("codemode")
            .is_some();
        if let Some(instances) = listed.as_array_mut() {
            instances.sort_by(|left, right| {
                (left["server"].as_str(), left["id"].as_str())
                    .cmp(&(right["server"].as_str(), right["id"].as_str()))
            });
            for instance in instances {
                if let Some(fields) = instance.as_object_mut() {
                    fields.shift_remove("status");
                    fields.shift_remove("error");
                }
            }
        }
        let next = json!([listed, locations, session, codemode]);
        if previous != next {
            let offer = Offer {
                servers: serde_json::from_value(listed)?,
                leases: serde_json::from_value(locations)?,
                session,
                codemode,
            };
            worker
                .registries
                .extensions
                .install(extension(offer, held.clone(), worker.clone()).map_err(invalid)?)?;
            previous = next;
        }
        tokio::select! {
            _ = changes.changed() => {},
            _ = settings.changed() => {},
            _ = moves.recv() => {},
        }
    }
}

fn subagents_document() -> DocToken {
    Arc::new(DocDefinition {
        kind: SUBAGENTS.into(),
        version: 2,
        semantics: DocumentSemantics::LatestConversation {
            fork: LatestFork::Initial,
        },
        initial: InitialDocument::Singleton(Arc::new(|| Ok(Default::default()))),
        // The only fields read are conversationId and archived, present in both versions.
        migrate: Some(Arc::new(|value, version| {
            if version == 1 {
                Ok(value)
            } else {
                Err("Unknown subagent document version".into())
            }
        })),
        checkpoint_when: Arc::new(|_, _, _| false),
    })
}

async fn work_locations(
    worker: &WorkerProcess,
    session: &pi_rs_durable::Session,
    ctx: &Context,
) -> Result<Vec<Location>> {
    let definition = subagents_document();
    let address =
        resolve_address(&definition, Some(ROOT_CONVERSATION_ID), None).map_err(invalid)?;
    let subagents = session
        .snapshot(&definition, &address, ctx)
        .await
        .map_err(invalid)?;
    let active = subagents
        .as_ref()
        .and_then(|doc| doc.get("agents"))
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|agents| agents.values())
        .filter(|agent| agent["archived"] != true)
        .filter_map(|agent| agent["conversationId"].as_u64());
    let mut locations = Vec::new();
    for conversation in std::iter::once(ROOT_CONVERSATION_ID).chain(active) {
        let location = locate(worker, conversation, Some(session), ctx)
            .await
            .map_err(invalid)?;
        if !locations
            .iter()
            .any(|other| same_location(other, &location))
        {
            locations.push(location);
        }
    }
    Ok(locations)
}

async fn project_files(
    worker: &WorkerProcess,
    locations: &[Location],
    ctx: &Context,
) -> Result<Vec<Value>> {
    let parts = worker.harness()?;
    let mut files = Vec::new();
    for location in locations {
        let env = parts
            .environments
            .lock()
            .await
            .at(location)
            .map_err(invalid)?;
        let file = async {
            let path = env
                .join_path(&[&location.cwd, ".pi", "mcp.json"], ctx)
                .await?;
            env.read_text_file(&path, ctx).await
        }
        .await;
        match file {
            Ok(text) if text.encode_utf16().count() <= PROJECT_LIMIT => files.push(json!({"location":location,"text":text})),
            Ok(_) => files.push(json!({"location":location,"error":format!("larger than {PROJECT_LIMIT} characters")})),
            Err(error) if error.code == FileErrorCode::NotFound => {},
            Err(error) => files.push(json!({"location":location,"error":error.message.chars().take(2000).collect::<String>()})),
        }
    }
    Ok(files)
}

fn invalid(error: impl std::fmt::Display) -> Error {
    Error::Invalid(error.to_string())
}
