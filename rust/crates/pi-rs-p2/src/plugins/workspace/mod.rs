//! Workspace views and binary transfers in the conversation's execution environment.

mod environment;
mod git;
mod server;

use std::sync::Arc;

use pi_rs_chord::{
    Result,
    context::Context,
    data::ChordError,
    host::{Facet, Members, ServeOptions, define_facet},
    json::JsonValue,
};
use pi_rs_durable::env::{FileChange, WatchExclude, WatchTarget};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{Mutex, Notify};

use crate::{
    api::v1::{
        FacetKind, PluginRegistry, Principal,
        p2_contracts::lookup,
        worker::{WorkerProcess, worker_of},
    },
    hosts::worker::Activity,
    runtime::{session_worker::Location, worker_environment::conversation_environment},
};

pub use environment::Workspace;
use environment::{Upload, invalid};
use git::Query;

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    registry.register("workspace", FacetKind::Server, server::facet()?)?;
    registry.register("workspace", FacetKind::Worker, worker()?)
}

async fn current(worker: &WorkerProcess, id: u64, ctx: &Context) -> Result<(Location, Workspace)> {
    let harness = worker.harness()?;
    harness
        .durable
        .conversation(id, ctx)
        .await
        .map_err(invalid)?
        .ok_or_else(|| invalid("Unknown conversation"))?;
    let (location, env) = {
        let mut execution = harness.environments.lock().await;
        conversation_environment(&mut execution, id, ctx, None)
            .await
            .map_err(invalid)?
    };
    Ok((location, Workspace::open(env, ctx).await?))
}

async fn write(
    worker: &WorkerProcess,
    location: &Location,
    upload: Upload,
    ctx: &Context,
) -> Result<JsonValue> {
    ctx.check().map_err(invalid)?;
    let harness = worker.harness()?;
    let env = harness
        .environments
        .lock()
        .await
        .at(location)
        .map_err(invalid)?;
    Workspace::open(env, ctx).await?.write(upload, ctx).await?;
    Ok(JsonValue::Null)
}

async fn watch(
    worker: Arc<WorkerProcess>,
    id: u64,
    target: pi_rs_chord::state::Source,
    ctx: Context,
) {
    let _viewer = worker.hold_activity(Activity::Viewer);
    let mut moves = worker.location.subscribe();
    let mut version = 0_u64;
    loop {
        let Ok((_, workspace)) = current(&worker, id, &ctx).await else {
            if ctx.await_with(moves.recv()).await.ok().flatten().is_some() {
                continue;
            }
            return;
        };
        let changes = Arc::new(Notify::new());
        let received = changes.clone();
        let mut targets = vec![WatchTarget {
            path: workspace.root.clone(),
            recursive: true,
            exclude: Some(WatchExclude {
                names: vec![".git".into(), "node_modules".into()],
                ..Default::default()
            }),
        }];
        if let Ok(run) = workspace
            .git(
                &[
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-dir",
                    "--git-common-dir",
                ],
                8192,
                &ctx,
            )
            .await
            && run.code == 0
        {
            let mut dirs = run.stdout.lines();
            if let Some(dir) = dirs.next() {
                for path in [
                    format!("{dir}/HEAD"),
                    format!("{dir}/index"),
                    format!("{}/config", dirs.next().unwrap_or(dir)),
                ] {
                    targets.push(WatchTarget {
                        path,
                        ..Default::default()
                    });
                }
            }
        }
        let mut watcher = workspace
            .env
            .watch(
                &targets,
                Box::new(move |change| {
                    if !matches!(change, FileChange::Failed { .. }) {
                        received.notify_one();
                    }
                }),
                &ctx,
            )
            .await;
        if watcher.is_err() && ctx.check().is_ok() {
            targets[0].recursive = false;
            let received = changes.clone();
            watcher = workspace
                .env
                .watch(&targets, Box::new(move |_| received.notify_one()), &ctx)
                .await;
        }
        loop {
            version = version.saturating_add(1);
            if target.set(json!(version).into(), &ctx).is_err() {
                return;
            }
            let moved = ctx
                .await_with(async {
                    tokio::select! {
                        moved = moves.recv() => moved.is_some(),
                        _ = changes.notified() => {
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            false
                        }
                    }
                })
                .await;
            if !matches!(moved, Ok(false)) {
                if let Ok(mut watcher) = watcher {
                    watcher.stop(&Context::background()).await;
                }
                if matches!(moved, Ok(true)) {
                    break;
                }
                return;
            }
        }
    }
}

fn worker() -> Result<Facet> {
    let backend = Arc::new(lookup("p2.workspace-backend")?);
    let writes = Arc::new(lookup("p2.workspace-writes")?);
    let workspace = Arc::new(lookup("p2.workspace")?);
    let transfer = Arc::new(lookup("p2.workspace-transfer")?);
    Ok(define_facet(move |host| {
        let worker = worker_of(&host.host())?;
        let backend_for_remote = backend.clone();
        let writes_for_backend = writes.clone();
        let service = worker.clone();
        let serial = Arc::new(Mutex::new(()));
        host.serve(
            writes.clone(),
            move |_, _, _| {
                let worker = service.clone();
                let serial = serial.clone();
                let mut members = Members::new();
                members.method("write", move |args, ctx| {
                    let serial = serial.clone();
                    let worker = worker.clone();
                    Box::pin(async move {
                        let args = args.to_serde()?;
                        let location = serde_json::from_value(args["location"].clone())?;
                        let upload = serde_json::from_value(args["upload"].clone())?;
                        let _permit = ctx
                            .context
                            .await_with(serial.lock())
                            .await
                            .map_err(invalid)?;
                        write(&worker, &location, upload, &ctx.context).await
                    })
                })?;
                Ok(members)
            },
            ServeOptions::default(),
        )?;

        host.serve(
            backend.clone(),
            move |params, _, object| {
                let id = params.to_serde()?["conversation"]
                    .as_u64()
                    .ok_or_else(|| invalid("Invalid conversation ID"))?;
                let writes = object.get(writes_for_backend.clone(), json!({}).into())?;
                let mut members = Members::new();
                let watching = worker.clone();
                let changes = object.follow(move |target, ctx| {
                    Box::pin(async move {
                        target.set(json!(0).into(), &ctx.context)?;
                        let (context, cancel) = Context::background().with_cancel();
                        tokio::spawn(watch(watching, id, target, context));
                        Ok(Box::new(move || cancel.cancel("workspace released"))
                            as Box<dyn FnOnce() + Send>)
                    })
                })?;
                members.state("changes", changes)?;
                let query_worker = worker.clone();
                members.method("query", move |args, ctx| {
                    let worker = query_worker.clone();
                    Box::pin(async move {
                        let query = serde_json::from_value(args.to_serde()?)?;
                        worker
                            .call(async {
                                let (location, workspace) =
                                    current(&worker, id, &ctx.context).await?;
                                let result = if matches!(query, Query::Place) {
                                    json!({"kind":"place","location":location})
                                } else {
                                    workspace.query(query, &ctx.context).await?
                                };
                                Ok(result.into())
                            })
                            .await
                    })
                })?;
                let read_worker = worker.clone();
                members.method("read", move |args, ctx| {
                    let worker = read_worker.clone();
                    Box::pin(async move {
                        let range = serde_json::from_value(args.to_serde()?)?;
                        worker
                            .call(async {
                                let (_, workspace) = current(&worker, id, &ctx.context).await?;
                                Ok(serde_json::to_value(
                                    workspace.read(range, &ctx.context).await?,
                                )?
                                .into())
                            })
                            .await
                    })
                })?;
                let write_worker = worker.clone();
                members.method("write", move |args, ctx| {
                    let worker = write_worker.clone();
                    let writes = writes.clone();
                    Box::pin(async move {
                        worker
                            .call(async {
                                let (location, _) = current(&worker, id, &ctx.context).await?;
                                writes
                                    .call(
                                        "write",
                                        json!({"location":location,"upload":args.to_serde()?})
                                            .into(),
                                        &ctx.context,
                                    )
                                    .await
                            })
                            .await
                    })
                })?;
                Ok(members)
            },
            ServeOptions::default(),
        )?;

        for (is_transfer, contract) in [(false, workspace.clone()), (true, transfer.clone())] {
            let backend = backend_for_remote.clone();
            host.serve(
                contract,
                move |params, principal, object| {
                    let params = params.to_serde()?;
                    let principal = principal.map(Principal::deserialize).transpose()?;
                    let session = params["session"].as_str().unwrap_or_default();
                    let admitted = if is_transfer {
                        principal == Some(Principal::Server)
                    } else {
                        principal.is_some_and(|principal| principal.can_send(session))
                    };
                    if !admitted {
                        let reason = if is_transfer {
                            "server only"
                        } else {
                            "participants only"
                        };
                        return Err(ChordError::new("forbidden", reason).into());
                    }
                    let backend = object.get(
                        backend.clone(),
                        json!({"conversation":params["conversation"]}).into(),
                    )?;
                    let mut members =
                        Members::variant(if is_transfer { "server" } else { "reader" });
                    if !is_transfer {
                        members
                            .state("changes", object.follow_source(backend.member("changes")?)?)?;
                    }
                    let methods: &[&str] = if is_transfer {
                        &["read", "write"]
                    } else {
                        &["query"]
                    };
                    for &name in methods {
                        let backend = backend.clone();
                        members.method(name, move |args, ctx| {
                            let backend = backend.clone();
                            Box::pin(async move { backend.call(name, args, &ctx.context).await })
                        })?;
                    }
                    Ok(members)
                },
                ServeOptions::default(),
            )?;
        }
        Ok(())
    }))
}
