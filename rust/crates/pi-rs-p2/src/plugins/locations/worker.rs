//! Worker facet for `locations`: installs location tools and drives deferred session moves.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use pi_rs_chord::{
    Result,
    context::Context,
    delta::Segment,
    host::{Facet, Host, define_facet},
    json::JsonValue,
};
use pi_rs_durable::{
    CommitChange, ConversationId, ROOT_CONVERSATION_ID,
    documents::resolve_address,
    session::{Error as DurableError, ObservedCommit, Session},
};
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::{
    api::v1::{
        p2_contracts::lookup,
        worker::{WorkerProcess, worker_of},
    },
    plugins::locations::{
        BoxedFuture, DirectoryListing, EnvInfo, LocationMoveState, LocationServer,
        location_move_doc,
        tool::{LocateConversation, create_location_extension},
    },
    runtime::{
        location::same_location, session_worker::Location, worker_environment::locate_conversation,
    },
};

const RETRY_DELAY: Duration = Duration::from_millis(2000);
const OPEN_POLL_DELAY: Duration = Duration::from_millis(25);

struct HostLocationServer {
    host: Host,
    contract: Arc<pi_rs_chord::contracts::Contract>,
}

impl LocationServer for HostLocationServer {
    fn envs<'a>(&'a self, context: &'a Context) -> BoxedFuture<'a, Vec<EnvInfo>> {
        Box::pin(async move {
            let held = self
                .host
                .get(
                    self.contract.clone(),
                    JsonValue::Object(Default::default()),
                    context,
                )
                .await?;
            let value = held
                .call("envs", JsonValue::Null, context)
                .await?
                .to_serde()?;
            let envs = value
                .get("envs")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new()));
            Ok(serde_json::from_value(envs)?)
        })
    }

    fn directories<'a>(
        &'a self,
        env: Option<&'a str>,
        path: &'a str,
        context: &'a Context,
    ) -> BoxedFuture<'a, DirectoryListing> {
        Box::pin(async move {
            let held = self
                .host
                .get(
                    self.contract.clone(),
                    JsonValue::Object(Default::default()),
                    context,
                )
                .await?;
            let value = held
                .call(
                    "directories",
                    JsonValue::from_serde(json!({ "env": env, "path": path })),
                    context,
                )
                .await?
                .to_serde()?;
            Ok(serde_json::from_value(value)?)
        })
    }
}

struct WorkerLocator {
    worker: Arc<WorkerProcess>,
}

impl LocateConversation for WorkerLocator {
    fn locate<'a>(
        &'a self,
        id: ConversationId,
        session: &'a Session,
        context: &'a Context,
    ) -> BoxedFuture<'a, Location> {
        Box::pin(async move {
            let parts = self.worker.harness()?;
            let env = parts.environments.lock().await;
            locate_conversation(Some(&*env), id, context, Some(session))
                .await
                .map_err(|e| pi_rs_chord::Error::Invalid(e.to_string()))
        })
    }
}

async fn read_pending(
    session: &Session,
    context: &Context,
) -> std::result::Result<Option<Location>, DurableError> {
    let doc = location_move_doc();
    let address = resolve_address(&doc, Some(ROOT_CONVERSATION_ID), None)?;
    let Some(snapshot) = session.snapshot(&doc, &address, context).await? else {
        return Ok(None);
    };
    let state: LocationMoveState = serde_json::from_value(Value::Object((*snapshot).clone()))?;
    Ok(state.pending)
}

fn commit_touches_location_move(commit: &ObservedCommit) -> bool {
    let changes = match commit {
        ObservedCommit::Stored(stored) => &stored.changes,
        ObservedCommit::Memory(changes) => changes,
    };
    changes.iter().any(|change| {
        matches!(
            change,
            CommitChange::Document(doc)
                if matches!(
                    doc,
                    pi_rs_durable::DocumentCommitChange::Change { record, .. }
                        if record.kind == "p2.location-move"
                )
        )
    })
}

async fn run_location_mover(
    worker: Arc<WorkerProcess>,
    host: Host,
    contract: Arc<pi_rs_chord::contracts::Contract>,
) {
    let parts = loop {
        if worker.stopping() {
            return;
        }
        if let Ok(parts) = worker.harness() {
            break parts;
        }
        tokio::time::sleep(OPEN_POLL_DELAY).await;
    };

    let ctx = Context::background();
    let durable = parts.durable.clone();
    let waiting = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(Notify::new());
    let waiting_flag = waiting.clone();
    let notify_commit = notify.clone();
    let Ok(_sub) = durable.session.follow_commits(move |commit, _| {
        if waiting_flag.load(Ordering::Acquire) || commit_touches_location_move(commit) {
            notify_commit.notify_one();
        }
        Ok(())
    }) else {
        return;
    };

    while !worker.stopping() {
        let pending = match read_pending(&durable.session, &ctx).await {
            Ok(p) => p,
            Err(_) if worker.stopping() => return,
            Err(err) => {
                eprintln!("p2 locations: the deferred move failed, retrying: {err}");
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
        };
        waiting.store(pending.is_some(), Ordering::Release);
        let Some(pending) = pending else {
            tokio::select! {
                () = notify.notified() => {}
                () = tokio::time::sleep(RETRY_DELAY) => {}
            }
            continue;
        };

        let inspection = match durable.inspect(&ctx).await {
            Ok(ins) => ins,
            Err(_) if worker.stopping() => return,
            Err(err) => {
                eprintln!("p2 locations: the deferred move failed, retrying: {err}");
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
        };
        if !inspection.tasks.is_empty() || !inspection.submissions.is_empty() {
            tokio::select! {
                () = notify.notified() => {}
                () = tokio::time::sleep(RETRY_DELAY) => {}
            }
            continue;
        }

        let failure: Option<String> = match async {
            let held = host
                .get(
                    contract.clone(),
                    JsonValue::Object(Default::default()),
                    &ctx,
                )
                .await?;
            let res = held
                .call(
                    "move",
                    JsonValue::from_serde(json!({
                        "env": pending.env,
                        "cwd": pending.cwd,
                    })),
                    &ctx,
                )
                .await?
                .to_serde()?;
            Ok::<Option<String>, pi_rs_chord::Error>(
                res.get("refused")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            )
        }
        .await
        {
            Ok(Some(_refused)) => {
                tokio::select! {
                    () = notify.notified() => {}
                    () = tokio::time::sleep(RETRY_DELAY) => {}
                }
                continue;
            }
            Ok(None) => None,
            Err(_) if worker.stopping() => return,
            Err(err) => Some(err.to_string()),
        };

        let doc = location_move_doc();
        let Ok(address) = resolve_address(&doc, Some(ROOT_CONVERSATION_ID), None) else {
            return;
        };
        let pending_cmp = pending.clone();
        let failure_msg = failure.clone();
        let _ = durable
            .session
            .commit(
                async move |tx| {
                    let handle = tx.doc(&doc, &address, None).await?;
                    let state_val: Value = handle
                        .value()
                        .map_err(|e| DurableError::invalid(e.to_string()))?
                        .to_json()
                        .to_serde()?;
                    let state: LocationMoveState = serde_json::from_value(state_val)?;
                    if state
                        .pending
                        .as_ref()
                        .is_some_and(|p| same_location(p, &pending_cmp))
                    {
                        handle.delete(&[Segment::Key("pending".into())])?;
                        if let Some(err) = failure_msg {
                            handle.set(
                                &[Segment::Key("failed".into())],
                                json!({ "to": pending_cmp, "error": err }).into(),
                            )?;
                        } else {
                            handle.delete(&[Segment::Key("failed".into())])?;
                        }
                    }
                    Ok(())
                },
                &ctx,
            )
            .await;
        if let Some(err) = failure {
            eprintln!("p2 locations: moving the session failed: {err}");
        }
    }
}

pub fn worker_facet() -> Result<Facet> {
    let agent_locations = Arc::new(lookup("p2.agent-locations")?);

    Ok(define_facet(move |facet_host| {
        let host = facet_host.host();
        let worker = worker_of(&host)?;

        let server = Arc::new(HostLocationServer {
            host: host.clone(),
            contract: agent_locations.clone(),
        });
        let locate = Arc::new(WorkerLocator {
            worker: worker.clone(),
        });
        let extension = create_location_extension(server, locate);
        worker.registries.extensions.install(extension)?;

        let mover_worker = worker.clone();
        let mover_host = host.clone();
        let mover_contract = agent_locations.clone();
        tokio::spawn(run_location_mover(mover_worker, mover_host, mover_contract));
        Ok(())
    }))
}
