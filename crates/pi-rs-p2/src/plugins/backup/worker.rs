use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use indexmap::IndexMap;
use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::Contract,
    host::{Facet, Held, Members, ServeOptions, define_facet},
    remote::{RemotePeer, RemoteReference},
    state::Source,
    sync::{
        AsyncRemote, BatchInfo, Body, FlushScheduler, PutBatchResult, RemoteListing, SyncFuture,
    },
};
use pi_rs_durable::{
    SubmissionStatus,
    session::{Observer, Subscription},
    storage_types::{CommitChange, CommitPublication},
};
use serde_json::{Value, json};

use crate::{
    hosts::worker::worker_of,
    runtime::{
        host_dirs::session_directory,
        logs::{LogLevel, process_logs},
        server::service_of,
    },
};

use super::{BackupStore, Target, is_terminal, paused_while_off};

#[derive(Default)]
struct WorkerBackup {
    closing: bool,
    live: Option<(FlushScheduler, Subscription)>,
}

#[derive(Clone)]
struct ChunkedRemote {
    store: String,
    reference: BackupReference,
}

#[derive(Clone)]
enum BackupReference {
    Local(Held),
    Remote(RemoteReference),
}

impl BackupReference {
    fn member(&self, name: &str) -> Result<Source> {
        match self {
            Self::Local(reference) => reference.member(name),
            Self::Remote(reference) => reference.member(name),
        }
    }

    async fn wait(&self, context: &Context) -> Result<()> {
        match self {
            Self::Local(reference) => reference.wait(context).await,
            Self::Remote(reference) => reference.wait(context).await,
        }
    }
}

impl ChunkedRemote {
    async fn call(&self, method: &str, args: Value) -> Result<Value> {
        let context = Context::background();
        let result = match &self.reference {
            BackupReference::Local(reference) => {
                reference.call(method, args.into(), &context).await
            }
            BackupReference::Remote(reference) => {
                reference.call(method, args.into(), &context).await
            }
        }?;
        result.to_serde()
    }
}

impl AsyncRemote for ChunkedRemote {
    fn list(&self, _: String) -> SyncFuture<RemoteListing> {
        Box::pin(async { Err(Error::Invalid("Worker backup remote only uploads".into())) })
    }

    fn get_batch(&self, _: String, _: u64) -> SyncFuture<Option<Body>> {
        Box::pin(async { Err(Error::Invalid("Worker backup remote only uploads".into())) })
    }

    fn get_snapshot(&self, _: String, _: u64) -> SyncFuture<Option<Body>> {
        Box::pin(async { Err(Error::Invalid("Worker backup remote only uploads".into())) })
    }

    fn put_batch(
        &self,
        id: String,
        batch: BatchInfo,
        mut body: Body,
    ) -> SyncFuture<PutBatchResult> {
        let remote = self.clone();
        Box::pin(async move {
            if id != remote.store {
                return Err(Error::Invalid(format!(
                    "A worker uploads only its own store, not {id}"
                )));
            }
            let mut slot = json!({"after": batch.after, "to": batch.to});
            if batch.archive == Some(true) {
                slot["archive"] = json!(true);
            }
            let begun = remote.call("begin", slot).await?;
            if begun["kind"].as_str() == Some("result") {
                return parse_result(&begun["result"]);
            }
            let upload = begun["upload"]
                .as_str()
                .ok_or_else(|| Error::Invalid("Missing backup upload ID".into()))?;
            let transfer = async {
                while let Some(part) = body.next_chunk().await {
                    let part = part?;
                    for bytes in part.chunks(super::CHUNK_BYTES) {
                        remote
                            .call("append", json!({"upload": upload, "bytes": bytes}))
                            .await?;
                    }
                }
                Ok::<_, Error>(())
            };
            if let Err(error) = transfer.await {
                let _ = remote.call("abort", json!({"upload": upload})).await;
                return Err(error);
            }
            parse_result(&remote.call("finish", json!({"upload": upload})).await?)
        })
    }
}

fn parse_result(result: &Value) -> Result<PutBatchResult> {
    let invalid = || Error::Invalid("Invalid backup upload result".into());
    match result["type"].as_str() {
        Some("created") => Ok(PutBatchResult::Created),
        Some("exists") => Ok(PutBatchResult::Exists {
            to: result["to"].as_u64().ok_or_else(invalid)?,
            device: result["device"].as_str().ok_or_else(invalid)?.into(),
        }),
        Some("not_head") => Ok(PutBatchResult::NotHead {
            head: result["head"].as_u64().ok_or_else(invalid)?,
        }),
        _ => Err(invalid()),
    }
}

pub(super) fn facet(remote: Arc<Contract>) -> Result<Facet> {
    let token = Arc::new(Contract::local(
        "p2.store-backup",
        1,
        Vec::new(),
        IndexMap::new(),
    )?);
    Ok(define_facet(move |facet| {
        let remote = remote.clone();
        facet.serve(
            token.clone(),
            move |_, _, object| {
                let host = object.host();
                let remote = remote.clone();
                let backup = Arc::new(Mutex::new(WorkerBackup::default()));
                let closing = backup.clone();
                object.open(
                    |_| Box::pin(async { Ok(()) }),
                    move |_, _| {
                        Box::pin(async move {
                            let live = {
                                let mut backup =
                                    closing.lock().unwrap_or_else(|error| error.into_inner());
                                backup.closing = true;
                                backup.live.take()
                            };
                            if let Some((schedule, observation)) = live {
                                drop(observation);
                                if let Err(error) = schedule.close().await {
                                    process_logs().append(
                                        "backup:worker",
                                        LogLevel::Event,
                                        &error.to_string(),
                                    );
                                }
                            }
                            Ok(())
                        })
                    },
                );
                object.on_live(move |ctx| {
                    let host = host.clone();
                    let remote = remote.clone();
                    let running = backup.clone();
                    Box::pin(async move {
                        let process = worker_of(&host)?;
                        let session = process.init.session()?.id;
                        let store_dir = session_directory(&process.init.data_dir, &session)?;
                        let context = Context::background();
                        let hold_remote = || match service_of::<RemotePeer>(&host, "p2.server-peer")
                        {
                            Ok(peer) => Ok(BackupReference::Remote(peer.hold(
                                remote.clone(),
                                json!({}).into(),
                                &context,
                            )?)),
                            Err(_) => Ok::<_, Error>(BackupReference::Local(host.hold(
                                remote.clone(),
                                json!({}).into(),
                                &context,
                            )?)),
                        };
                        let Some(target) = Target::configured()? else {
                            if let Some(paused) = paused_while_off(&store_dir).await?
                                && let Ok(held) = hold_remote()
                                && held.wait(&ctx.context).await.is_ok()
                            {
                                let worker_remote = ChunkedRemote {
                                    store: format!("session-{session}"),
                                    reference: held,
                                };
                                let _ = worker_remote
                                    .call("report", json!({"status": paused}))
                                    .await;
                            }
                            return Ok(());
                        };
                        let store_name = format!("session-{session}");
                        let Some(abort) = ctx.context.abort_signal() else {
                            return Ok(());
                        };
                        let parts = loop {
                            if let Ok(parts) = process.harness() {
                                break parts;
                            }
                            tokio::select! {
                                _ = abort.cancelled() => return Ok(()),
                                _ = tokio::time::sleep(Duration::from_millis(50)) => {},
                            }
                        };
                        let held = hold_remote()?;
                        held.wait(&ctx.context).await?;
                        let device = held.member("device")?.value()?.to_json().to_serde()?;
                        let Some(device) = device.as_str() else {
                            return Ok(());
                        };
                        let remote = Arc::new(ChunkedRemote {
                            store: store_name.clone(),
                            reference: held,
                        });
                        let account_remote = remote.clone();
                        let backup = Arc::new(
                            BackupStore::open(
                                parts.store.clone(),
                                store_name,
                                &store_dir,
                                &target.key(),
                                remote.clone(),
                                device.into(),
                            )
                            .await?
                            .with_terminal(Some(Arc::new(is_terminal)))
                            .with_account(Some(Arc::new(move || {
                                let remote = account_remote.clone();
                                Box::pin(async move {
                                    match remote.call("account", json!({})).await? {
                                        Value::String(account) => Ok(Some(account)),
                                        Value::Null => Ok(None),
                                        _ => Err(Error::Invalid(
                                            "Invalid backup account response".into(),
                                        )),
                                    }
                                })
                            }))),
                        );
                        let schedule = FlushScheduler::new(
                            Default::default(),
                            move || {
                                let backup = backup.clone();
                                let remote = remote.clone();
                                Box::pin(async move {
                                    let statuses = Mutex::new(Vec::new());
                                    let result = backup
                                        .run(|status| {
                                            statuses
                                                .lock()
                                                .unwrap_or_else(|error| error.into_inner())
                                                .push(status)
                                        })
                                        .await;
                                    for status in statuses
                                        .into_inner()
                                        .unwrap_or_else(|error| error.into_inner())
                                    {
                                        let _ =
                                            remote.call("report", json!({"status": status})).await;
                                    }
                                    result
                                })
                            },
                            |_| {},
                        );
                        let notify = schedule.clone();
                        let observation = parts
                            .durable
                            .session
                            .observe_commits(Observer::new(
                                move |commit: &CommitPublication, _| {
                                    let urgent = commit.changes.iter().any(|change| match change {
                                        CommitChange::Entry { value } => matches!(
                                            value.kind.as_str(),
                                            "p2.input" | "pi.user" | "pi.assistant"
                                        ),
                                        CommitChange::Submission { value } => matches!(
                                            value.status,
                                            SubmissionStatus::Done | SubmissionStatus::Unanswered
                                        ),
                                        _ => false,
                                    });
                                    notify.notify(urgent);
                                    Ok(())
                                },
                                |_| {},
                            ))
                            .map_err(|error| Error::Invalid(error.to_string()))?;
                        let mut running = running.lock().unwrap_or_else(|error| error.into_inner());
                        if !running.closing {
                            schedule.notify(false);
                            running.live = Some((schedule, observation));
                        }
                        Ok(())
                    })
                });
                Ok(Members::new())
            },
            ServeOptions::default(),
        )
    }))
}
