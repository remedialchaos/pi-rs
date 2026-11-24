use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::{Contract, LOCAL, schema},
    data::ChordError,
    delta::Revision,
    documents::Definition,
    host::{Caller, FacetHost, Held, JobBinding, Members, ObjectSelf, ServeOptions, WeakHost},
    jobs::{
        JobContext, JobImplementation, JobOptions, JobResult, Outcome, Phase, Resume, RunningEntry,
        define_job,
    },
    json::JsonValue,
    remote::RemoteReference,
    state::{Batch, Source},
    watch::NativeWatchCall,
};
use serde_json::{Map, Value, json};
use tokio::sync::watch;

use crate::{
    api::v1::{
        node::session_directory,
        p2_contracts::lookup,
        server::{
            ServerNatives, SessionSuspendGuard, SessionWorkers, WORKER_INIT_PROVIDER_SERVICE,
            WorkerInitProvider, install_service, server_of, service_of,
        },
        worker::worker_boundary_contract,
    },
    plugins::shard_a::{LoginResources, MODEL_RESOURCES},
};

use super::{arg, clean_title, copy, truncate, wire};

#[derive(Clone)]
struct Lifecycle {
    host: WeakHost,
    catalog: Held,
    preparing: watch::Sender<BTreeSet<String>>,
    archived: Source,
    suspended: ArchivedWorkers,
    transfer: Arc<Contract>,
    boundary: Arc<Contract>,
    moves: Arc<tokio::sync::Mutex<()>>,
}

pub(super) fn serve(host: &mut FacetHost, catalog: Arc<Contract>) -> Result<()> {
    let operations = Arc::new(lookup("p2.session-operations")?);
    let moves = Arc::new(lookup("p2.session-moves")?);
    let archived = Arc::new(lookup("p2.archived-starts")?);
    let suspended = ArchivedWorkers::default();
    let indexing = suspended.clone();
    host.serve(
        archived.clone(),
        move |_, _, object| {
            let allowed = Source::new(object.batch(), wire(json!([])))?;
            let update = Arc::new(Mutex::new(BTreeSet::<String>::new()));
            let mut members = Members::new();
            members.state("allowed", allowed.clone())?;
            for method in ["allow", "end"] {
                let (allowed, update, host, indexing) = (
                    allowed.clone(),
                    update.clone(),
                    object.host().downgrade(),
                    indexing.clone(),
                );
                members.method(method, move |args, ctx| {
                    let (allowed, update, host, indexing) = (
                        allowed.clone(),
                        update.clone(),
                        host.clone(),
                        indexing.clone(),
                    );
                    Box::pin(async move {
                        let args = args.to_serde()?;
                        let id = arg(&args, "session")?;
                        {
                            let mut allowed_ids =
                                update.lock().unwrap_or_else(|error| error.into_inner());
                            if method == "allow" {
                                allowed_ids.insert(id.to_owned());
                            } else {
                                allowed_ids.remove(id);
                            }
                            allowed.set(wire(json!(*allowed_ids)), &ctx.context)?;
                        }
                        let server = server_of(&host.upgrade().ok_or(Error::Closed)?)?;
                        if method == "allow" {
                            indexing.resume(&server.workers, id).await?;
                        } else {
                            let catalog = server
                                .host()
                                .get(
                                    Arc::new(lookup("p2.catalog")?),
                                    wire(json!({})),
                                    &ctx.context,
                                )
                                .await?;
                            let row = catalog
                                .call("get", wire(json!({"id":id})), &ctx.context)
                                .await;
                            catalog.release();
                            if !row?.to_serde()?["archivedAt"].is_null() {
                                indexing.suspend(&server.workers, id).await?;
                            }
                        }
                        Ok(JsonValue::Null)
                    })
                })?;
            }
            Ok(members)
        },
        ServeOptions::default(),
    )?;

    let definitions = operations.clone();
    host.serve(
        operations.clone(),
        move |_, _, object| {
            let service = Lifecycle {
                host: object.host().downgrade(),
                catalog: object.get(catalog.clone(), wire(json!({})))?,
                preparing: watch::channel(BTreeSet::new()).0,
                archived: object
                    .get(archived.clone(), wire(json!({})))?
                    .member("allowed")?,
                suspended: suspended.clone(),
                transfer: Arc::new(lookup("p2.session-transfer")?),
                boundary: worker_boundary_contract()?,
                moves: Arc::new(tokio::sync::Mutex::new(())),
            };
            let failures = Failures::new(object.batch())?;
            let clones = job(
                object,
                &definitions,
                service.clone(),
                failures.clone(),
                "clone",
            )?;
            let forks = job(
                object,
                &definitions,
                service.clone(),
                failures.clone(),
                "fork",
            )?;
            let archives = job(
                object,
                &definitions,
                service.clone(),
                failures.clone(),
                "archive",
            )?;
            let moves = job(
                object,
                &definitions,
                service.clone(),
                failures.clone(),
                "relocate",
            )?;
            let list = Source::derive(
                object.batch(),
                vec![
                    moves.source,
                    clones.source,
                    forks.source,
                    archives.source,
                    failures.source.clone(),
                ],
                |values| {
                    let entries = values
                        .iter()
                        .map(|value| {
                            Ok(serde_json::from_value::<Vec<Value>>(
                                value.to_json().to_serde()?,
                            )?)
                        })
                        .collect::<Result<Vec<_>>>()?;
                    Ok(Revision::from_json(wire(json!(
                        entries.into_iter().flatten().collect::<Vec<_>>()
                    )))?)
                },
                |_| {},
            );
            let (start, for_start, forks_for_start, clones_for_start) = (
                service.clone(),
                list.clone(),
                forks.binding.clone(),
                clones.binding.clone(),
            );
            object.open(
                move |_| {
                    Box::pin(async move {
                        let host = start.host.upgrade().ok_or(Error::Closed)?;
                        let provider: WorkerInitProvider = Arc::new(move |id, _, context| {
                            let (start, for_start, forks, clones) = (
                                start.clone(),
                                for_start.clone(),
                                forks_for_start.clone(),
                                clones_for_start.clone(),
                            );
                            Box::pin(async move {
                                start
                                    .worker_init(&id, &context, &for_start, &forks, &clones)
                                    .await
                            })
                        });
                        let provider = Arc::new(provider);
                        install_service(&host, WORKER_INIT_PROVIDER_SERVICE, &provider)?;
                        Ok(provider)
                    })
                },
                |_, _| Box::pin(async { Ok(()) }),
            );

            let mut members = Members::new();
            members.state("operations", list)?;
            for method in [
                "create", "move", "relocate", "clone", "fork", "archive", "restore", "dismiss",
            ] {
                let service = service.clone();
                let jobs = (
                    clones.binding.clone(),
                    forks.binding.clone(),
                    archives.binding.clone(),
                    moves.binding.clone(),
                );
                let failures = failures.clone();
                members.method(method, move |args, ctx| {
                    let (service, jobs, failures) =
                        (service.clone(), jobs.clone(), failures.clone());
                    Box::pin(async move {
                        let args: Value = args.to_serde()?;
                        let context = &ctx.context;
                        match method {
                            "create" => service.create(&args, context).await,
                            "clone" | "fork" => {
                                service
                                    .start_copy(
                                        method,
                                        args,
                                        if method == "clone" { jobs.0 } else { jobs.1 },
                                        context,
                                    )
                                    .await
                            }
                            "archive" => {
                                let _moves = service.moves.lock().await;
                                let id = arg(&args, "id")?;
                                service
                                    .catalog
                                    .call("update", wire(json!({"id":id,"archived":true})), context)
                                    .await?;
                                jobs.2
                                    .get()?
                                    .start(wire(args.clone()), Some(id.to_owned()), context)
                                    .await?;
                                Ok(JsonValue::Null)
                            }
                            "restore" => {
                                let _moves = service.moves.lock().await;
                                let id = arg(&args, "id")?;
                                let job = jobs.2.get()?;
                                if let Some(run) = job.running().into_iter().find(|entry| {
                                    entry.phase != Phase::Blocked
                                        && entry.key.as_deref() == Some(id)
                                }) {
                                    let _ = job.outcome(&run.id, context).await;
                                }
                                service
                                    .catalog
                                    .call(
                                        "update",
                                        wire(json!({"id":id,"archived":false})),
                                        context,
                                    )
                                    .await?;
                                service
                                    .suspended
                                    .resume(&service.server()?.workers, id)
                                    .await?;
                                Ok(JsonValue::Null)
                            }
                            "move" | "relocate" => {
                                let id = arg(&args, "id")?;
                                service.live(id, context).await?;
                                let cwd = local_directory(&args, false, false).await?;
                                let by = if method == "move" { "agent" } else { "owner" };
                                let run = jobs
                                    .3
                                    .get()?
                                    .start(
                                        wire(json!({"id":id,"cwd":cwd,"env":args["env"],"by":by})),
                                        None,
                                        context,
                                    )
                                    .await?;
                                if method == "relocate" {
                                    return Ok(JsonValue::Null);
                                }
                                match run.outcome(context).await?.outcome {
                                    JobResult::Done { result } => Ok(result),
                                    JobResult::Failed { error } => {
                                        Err(Error::Invalid(failure_text(error)))
                                    }
                                }
                            }
                            _ => {
                                let id = arg(&args, "operation")?;
                                failures.change(context, |entries| {
                                    entries.retain(|entry| entry["id"] != id)
                                })?;
                                Ok(JsonValue::Null)
                            }
                        }
                    })
                })?;
            }
            Ok(members)
        },
        ServeOptions::default(),
    )?;
    host.serve(
        moves,
        move |_, _, object| {
            let operations = object.get(operations.clone(), wire(json!({})))?;
            let mut members = Members::new();
            members.method("relocate", move |args, ctx| {
                let operations = operations.clone();
                Box::pin(async move { operations.call("move", args, &ctx.context).await })
            })?;
            Ok(members)
        },
        ServeOptions::default(),
    )
}

struct LifecycleJob {
    binding: JobBinding,
    source: Source,
}

fn job(
    object: &mut ObjectSelf,
    operations: &Contract,
    service: Lifecycle,
    failures: Failures,
    name: &'static str,
) -> Result<LifecycleJob> {
    use schema::{arr, bool, nul, obj, required, str, union, unknown};
    let variant = operations
        .current()
        .variants
        .get(LOCAL)
        .ok_or(Error::Closed)?;
    let args = match name {
        "fork" => obj([
            ("id", required(str())),
            ("copy", required(str())),
            ("entries", required(arr(unknown()))),
        ]),
        "relocate" => obj([
            ("id", required(str())),
            ("cwd", required(str())),
            ("env", required(union([str(), nul()]))),
            (
                "by",
                required(wire(json!({"enum":["owner","agent"],"type":"string"}))),
            ),
        ]),
        _ => variant
            .methods
            .get(name)
            .ok_or(Error::Closed)?
            .args
            .schema
            .clone(),
    };
    let result = if matches!(name, "clone" | "relocate") {
        obj([("refused", required(union([str(), nul()])))])
    } else {
        nul()
    };
    let mut options = JobOptions::new(1, args, result);
    options.persist = Some(Resume::Rerun);
    if name == "fork" {
        options.state = Some(Arc::new(
            Definition::new(
                "p2.sessions.fork.progress",
                obj([("copying", required(bool()))]),
                1,
                || Ok(wire(json!({"copying":true}))),
            )?
            .persisted(true, |_, _, _| true),
        ));
    }
    let binding = object.job(
        define_job(format!("p2.sessions.{name}"), options)?,
        JobImplementation::new(move |args, ctx| {
            let service = service.clone();
            Box::pin(async move { service.run_job(name, args.to_serde()?, ctx).await })
        })
        .with_finish(move |outcome| {
            let failures = failures.clone();
            Box::pin(async move { failures.record(name, outcome) })
        }),
    )?;
    let source = Source::new(object.batch(), wire(json!([])))?;
    let (running, publish) = (binding.clone(), source.clone());
    object.on_live(move |ctx| {
        let (running, publish) = (running.clone(), publish.clone());
        Box::pin(async move {
            let changed = publish.clone();
            let observation = running.get()?.running_source().observe(
                &ctx.context,
                move |event, context| {
                    if let NativeWatchCall::Value { value } = event {
                        changed.set(wire(running_entries(name, &value)?), context)?;
                    }
                    Ok(())
                },
                |_| {},
            )?;
            publish.set(
                wire(running_entries(name, &observation.value)?),
                &ctx.context,
            )?;
            std::future::pending::<()>().await;
            Ok(())
        })
    });
    Ok(LifecycleJob { binding, source })
}

fn running_entries(name: &str, runs: &[RunningEntry]) -> Result<Value> {
    Ok(json!(runs.iter().filter(|run| run.phase != Phase::Blocked).filter_map(|run| {
        let result = (|| {
            let args: Value = run.args.to_serde()?;
            let state: Value = run.state.as_ref().map(JsonValue::to_serde).transpose()?.unwrap_or(Value::Null);
            Ok((args["by"] != "agent").then(|| json!({
                "phase":"running","id":run.id,"kind":name,"session":args["id"],"target":args["copy"],
                "copying":name == "clone" || (name == "fork" && state["copying"] != false),
            })))
        })();
        match result {
            Ok(None) => None,
            Ok(Some(row)) => Some(Ok(row)),
            Err(error) => Some(Err(error)),
        }
    }).collect::<Result<Vec<_>>>()?))
}

#[derive(Clone)]
struct Failures {
    entries: Arc<Mutex<Vec<Value>>>,
    source: Source,
}

impl Failures {
    fn new(batch: Batch) -> Result<Self> {
        Ok(Self {
            entries: Arc::default(),
            source: Source::new(batch, wire(json!([])))?,
        })
    }

    fn change(&self, ctx: &Context, change: impl FnOnce(&mut Vec<Value>)) -> Result<()> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        change(&mut entries);
        self.source.set(wire(json!(*entries)), ctx)
    }

    fn record(&self, name: &str, outcome: Outcome) -> Result<()> {
        let args: Value = outcome.args.to_serde()?;
        let error = match outcome.outcome {
            JobResult::Failed { error } => Some(failure_text(error)),
            JobResult::Done { result } if args["by"] != "agent" => {
                let result: Value = result.to_serde()?;
                result["refused"].as_str().map(str::to_owned)
            }
            _ => None,
        };
        if let Some(error) = error {
            self.change(&Context::background(), |entries| {
                entries.retain(|row| row["id"] != outcome.id);
                entries.push(json!({"phase":"failed","id":outcome.id,"kind":name,
                    "session":args["id"],"target":args["copy"],"error":error}));
                if entries.len() > 20 {
                    entries.remove(0);
                }
            })?;
        }
        Ok(())
    }
}

fn failure_text(error: pi_rs_chord::jobs::JobError) -> String {
    use pi_rs_chord::jobs::JobError;
    match error {
        JobError::Threw { error } => error["message"]
            .as_str()
            .map_or_else(|| error.to_string(), str::to_owned),
        JobError::Aborted { reason } => reason,
        JobError::RestartLimit { .. } => "The operation could not resume".into(),
    }
}

impl Lifecycle {
    fn server(&self) -> Result<Arc<ServerNatives>> {
        server_of(&self.host.upgrade().ok_or(Error::Closed)?)
    }

    async fn row(&self, id: &str, context: &Context) -> Result<Value> {
        self.catalog
            .call("get", wire(json!({"id":id})), context)
            .await?
            .to_serde()
    }

    async fn live(&self, id: &str, context: &Context) -> Result<Value> {
        let row = self.row(id, context).await?;
        if !row["archivedAt"].is_null() {
            return Err(ChordError::new("forbidden", "Session is archived").into());
        }
        Ok(row)
    }

    fn existing(&self, id: &str) -> Result<Option<Value>> {
        let rows: Vec<Value> = serde_json::from_value(self.catalog.read("sessions")?.to_serde()?)?;
        Ok(rows.into_iter().find(|row| row["id"] == id))
    }

    async fn create(&self, args: &Value, context: &Context) -> Result<JsonValue> {
        if let Some(id) = args["id"].as_str()
            && let Some(row) = self.existing(id)?
        {
            return Ok(wire(row));
        }
        let cwd = local_directory(args, true, args["createDirectory"] == true).await?;
        let server = self.server()?;
        let fallback = if server.host().served_contract("p2.model-config").is_some() {
            let config = server
                .host()
                .get(
                    Arc::new(lookup("p2.model-config")?),
                    wire(json!({})),
                    context,
                )
                .await?;
            let summary: Value = config.read("summary")?.to_serde()?;
            config.release();
            summary["defaultModel"].clone()
        } else if server.info.fake {
            json!({"provider":"faux","modelId":"faux-1","thinkingLevel":"high"})
        } else {
            Value::Null
        };
        let model = if args["model"].is_null() {
            &fallback
        } else {
            &args["model"]
        };
        if model.is_null() {
            return Err(Error::Invalid(
                "Select a model or configure P2_PROVIDER and P2_MODEL".into(),
            ));
        }
        let (provider, model_id) = (arg(model, "provider")?, arg(model, "modelId")?);
        let known = match service_of::<LoginResources>(server.host(), MODEL_RESOURCES) {
            Ok(resources) => resources.models.get_model(provider, model_id).is_some(),
            Err(_) => server.info.fake && provider == "faux" && model_id == "faux-1",
        };
        if !known {
            return Err(Error::Invalid("Unknown model".into()));
        }
        let level = args["thinkingLevel"]
            .as_str()
            .or_else(|| fallback["thinkingLevel"].as_str())
            .filter(|level| {
                matches!(
                    *level,
                    "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
                )
            })
            .unwrap_or("high");
        let title = truncate(
            &clean_title(args["title"].as_str().unwrap_or_default()),
            100,
        );
        let id = args["id"]
            .as_str()
            .map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_owned);
        let row = self.catalog.call("create", wire(json!({
            "id":id,"cwd":cwd,"env":null,"model":{"provider":provider,"modelId":model_id},"thinkingLevel":level,
            "title":if title.is_empty() { "New session" } else { &title }, "titleEdited":!title.is_empty(),
        })), context).await;
        if matches!(&row, Err(Error::Chord(error)) if error.code == "conflict")
            && let Some(existing) = self.existing(&id)?
        {
            return Ok(wire(existing));
        }
        row
    }

    async fn worker_init(
        &self,
        id: &str,
        context: &Context,
        operations: &Source,
        forks: &JobBinding,
        clones: &JobBinding,
    ) -> Result<Map<String, Value>> {
        let rows = self.catalog.member("sessions")?;
        let (wake, mut changed) = watch::channel(());
        let update = wake.clone();
        let _row_watch = rows.subscribe(
            move |_, _| {
                update.send_replace(());
                Ok(())
            },
            |_| {},
        );
        let _ = context
            .await_with(tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    changed.borrow_and_update();
                    if self.existing(id)?.is_some() {
                        return Ok::<_, Error>(());
                    }
                    changed.changed().await.map_err(|_| Error::Closed)?;
                }
            }))
            .await;
        let mut preparing = self.preparing.subscribe();
        let _operation_watch = operations.subscribe(
            move |_, _| {
                wake.send_replace(());
                Ok(())
            },
            |_| {},
        );
        context
            .await_with(async {
                loop {
                    changed.borrow_and_update();
                    if self
                        .existing(id)?
                        .is_some_and(|row| !row["archivedAt"].is_null())
                    {
                        break;
                    }
                    let making = preparing.borrow_and_update().contains(id);
                    let copying =
                        [("clone", clones), ("fork", forks)]
                            .into_iter()
                            .any(|(kind, job)| {
                                job.get().is_ok_and(|job| {
                                    job.running().iter().any(|run| {
                                        run.phase != Phase::Blocked
                                            && run.key.as_deref() == Some(id)
                                            && (kind == "clone"
                                                || run
                                                    .state
                                                    .as_ref()
                                                    .and_then(|state| state.get("copying"))
                                                    != Some(&JsonValue::Bool(false)))
                                    })
                                })
                            });
                    if !making && !copying {
                        break;
                    }
                    tokio::select! {
                        result = changed.changed() => result.map_err(|_| Error::Closed)?,
                        result = preparing.changed() => result.map_err(|_| Error::Closed)?,
                    }
                }
                Ok::<_, Error>(())
            })
            .await
            .map_err(|error| Error::Invalid(error.to_string()))??;
        let row = self.row(id, context).await?;
        let archived = !row["archivedAt"].is_null();
        if archived {
            let allowed: Vec<String> =
                serde_json::from_value(self.archived.value()?.to_json().to_serde()?)?;
            if !allowed.iter().any(|name| name == id) {
                return Err(ChordError::new("forbidden", "Session is archived").into());
            }
        }
        if let Some(env) = row["env"].as_str() {
            return Err(Error::Invalid(format!(
                "Environment {env:?} is not configured; refusing local execution"
            )));
        }
        let mut session = json!({"id":id,"cwd":row["cwd"],"env":row["env"],"model":row["model"],"thinkingLevel":row["thinkingLevel"]});
        if archived {
            session["resume"] = false.into();
        }
        Ok(Map::from_iter([("session".into(), session)]))
    }

    async fn start_copy(
        &self,
        kind: &str,
        mut args: Value,
        job: JobBinding,
        context: &Context,
    ) -> Result<JsonValue> {
        let id = arg(&args, "id")?.to_owned();
        let copy = arg(&args, "copy")?.to_owned();
        self.live(&id, context).await?;
        if let Some(existing) = self.existing(&copy)? {
            return Ok(wire(existing));
        }
        if kind == "fork" {
            let transfer = self.worker(&id, self.transfer.clone(), context).await?;
            let history: Value = transfer
                .0
                .call(
                    "history",
                    wire(json!({"before":args["before"],"conversation":args["conversation"]})),
                    context,
                )
                .await?
                .to_serde()?;
            args = json!({"id":id,"copy":copy,"entries":history["entries"]});
        }
        let _moves = self.moves.lock().await;
        let source = self.live(&id, context).await?;
        if let Some(existing) = self.existing(&copy)? {
            return Ok(wire(existing));
        }
        self.preparing.send_modify(|ids| {
            ids.insert(copy.clone());
        });
        let marked = Preparing {
            ids: self.preparing.clone(),
            copy: copy.clone(),
        };
        let mark = if kind == "clone" { "copy" } else { "fork" };
        let title = truncate(&format!("{} ({mark})", arg(&source, "title")?), 120);
        let row = self
            .catalog
            .call(
                "create",
                wire(json!({
                    "id":copy,"cwd":source["cwd"],"env":source["env"],"model":source["model"],
                    "thinkingLevel":source["thinkingLevel"],"title":title,"titleEdited":true,
                })),
                context,
            )
            .await?;
        if let Err(error) = job
            .get()?
            .start(wire(args), Some(copy.clone()), context)
            .await
        {
            self.catalog
                .call(
                    "update",
                    wire(json!({"id":copy,"archived":true})),
                    &Context::background(),
                )
                .await?;
            return Err(error);
        }
        drop(marked);
        Ok(row)
    }

    async fn worker(
        &self,
        id: &str,
        contract: Arc<Contract>,
        context: &Context,
    ) -> Result<WorkerRef> {
        let reference = self
            .server()?
            .workers
            .hold(
                contract,
                wire(json!({"session":id})),
                Caller {
                    principal: Some(json!({"kind":"server"})),
                    incarnation: uuid::Uuid::new_v4().to_string(),
                },
                context,
            )
            .await?;
        Ok(WorkerRef(reference))
    }

    async fn run_job(&self, name: &str, args: Value, ctx: JobContext) -> Result<JsonValue> {
        let id = arg(&args, "id")?;
        let context = &ctx.context;
        match name {
            "archive" => {
                let server = self.server()?;
                self.suspended.suspend(&server.workers, id).await?;
                Ok(JsonValue::Null)
            }
            "relocate" => {
                let _moves = self.moves.lock().await;
                self.live(id, context).await?;
                self.relocate(id, &args, context).await
            }
            _ => {
                let copy = arg(&args, "copy")?;
                let _moves = self.moves.lock().await;
                let result = if name == "clone" {
                    self.clone_session(id, copy, context).await
                } else {
                    self.fork_session(id, copy, &args["entries"], &ctx).await
                };
                if !context.is_aborted()
                    && (result.is_err()
                        || result.as_ref().is_ok_and(|value| {
                            value
                                .get("refused")
                                .is_some_and(|value| value != &JsonValue::Null)
                        }))
                {
                    self.fail_copy(copy, context).await?;
                }
                result
            }
        }
    }

    async fn relocate(&self, id: &str, args: &Value, context: &Context) -> Result<JsonValue> {
        let server = self.server()?;
        let location = json!({"cwd":args["cwd"],"env":args["env"]});
        let current = self.row(id, context).await?;
        if current["cwd"] == args["cwd"] && current["env"] == args["env"] {
            return Ok(wire(json!({"refused":null})));
        }
        let update = || {
            self.catalog.call(
                "update",
                wire(json!({"id":id,"cwd":args["cwd"],"env":args["env"]})),
                context,
            )
        };
        if server.workers.running().iter().any(|session| session == id) {
            let boundary = self.worker(id, self.boundary.clone(), context).await?;
            if let Some(refused) = prepare(
                &boundary,
                json!({"to":location}),
                &current,
                "changing directory",
                context,
            )
            .await?
            {
                return Ok(wire(json!({"refused":refused})));
            }
            let result = async {
                update().await?;
                boundary.0.call("relocate", wire(location), context).await
            }
            .await;
            if result.is_err() {
                let _ = boundary
                    .0
                    .call("cancel", JsonValue::Null, &Context::background())
                    .await;
                server.workers.stop(id).await?;
                result?;
            }
        } else {
            let guard = server.workers.suspend(id).await?;
            update().await?;
            guard.resume().await?;
        }
        Ok(wire(json!({"refused":null})))
    }

    async fn clone_session(&self, id: &str, copy_id: &str, context: &Context) -> Result<JsonValue> {
        let server = self.server()?;
        let row = self.live(id, context).await?;
        let boundary = if server.workers.running().iter().any(|session| session == id) {
            let boundary = self.worker(id, self.boundary.clone(), context).await?;
            if let Some(message) = prepare(&boundary, Value::Null, &row, "cloning", context).await?
            {
                return Ok(wire(json!({"refused":message})));
            }
            Some(boundary)
        } else {
            None
        };
        let paused = server.workers.suspend(id).await;
        if paused.is_err()
            && let Some(boundary) = &boundary
        {
            let _ = boundary
                .0
                .call("cancel", JsonValue::Null, &Context::background())
                .await;
        }
        let paused = paused?;
        let data = Path::new(&server.info.data_dir);
        let target = session_directory(data, copy_id)?;
        copy::remove(&target).await?;
        copy::copy(&session_directory(data, id)?, &target).await?;
        let row = self.row(id, context).await?;
        if server.host().served_contract("p2.artifact-store").is_some() {
            let _ = crate::plugins::artifacts::server::copy_session_artifacts(
                server.host(),
                id,
                copy_id,
                context,
            )
            .await;
        }
        self.catalog
            .call(
                "update",
                wire(json!({"id":copy_id,"cwd":row["cwd"],"env":row["env"]})),
                context,
            )
            .await?;
        paused.resume().await?;
        Ok(wire(json!({"refused":null})))
    }

    async fn fork_session(
        &self,
        id: &str,
        copy_id: &str,
        entries: &Value,
        ctx: &JobContext,
    ) -> Result<JsonValue> {
        if ctx.state()?.and_then(|state| state.get("copying").cloned())
            != Some(JsonValue::Bool(false))
        {
            let server = self.server()?;
            copy::remove(&session_directory(
                Path::new(&server.info.data_dir),
                copy_id,
            )?)
            .await?;
            let source = self.worker(id, self.transfer.clone(), &ctx.context).await?;
            source
                .0
                .call("copyFiles", wire(json!({"to":copy_id})), &ctx.context)
                .await?;
            if server.host().served_contract("p2.artifact-store").is_some() {
                let _ = crate::plugins::artifacts::server::copy_session_artifacts(
                    server.host(),
                    id,
                    copy_id,
                    &ctx.context,
                )
                .await;
            }
            ctx.change_state(|state| {
                *state = wire(json!({"copying":false}));
                Ok(())
            })
            .await?;
        }
        let target = self
            .worker(copy_id, self.transfer.clone(), &ctx.context)
            .await?;
        target
            .0
            .call("seed", wire(json!({"entries":entries})), &ctx.context)
            .await
    }

    async fn fail_copy(&self, id: &str, context: &Context) -> Result<()> {
        self.catalog
            .call("update", wire(json!({"id":id,"archived":true})), context)
            .await?;
        let server = self.server()?;
        self.suspended.suspend(&server.workers, id).await?;
        let data = Path::new(&server.info.data_dir);
        copy::remove(&session_directory(data, id)?).await?;
        copy::remove(&data.join("uploads").join(id)).await?;
        Ok(())
    }
}

#[derive(Clone, Default)]
struct ArchivedWorkers(Arc<tokio::sync::Mutex<BTreeMap<String, SessionSuspendGuard>>>);

impl ArchivedWorkers {
    async fn suspend(&self, workers: &SessionWorkers, id: &str) -> Result<()> {
        let mut suspended = self.0.lock().await;
        if !suspended.contains_key(id) {
            suspended.insert(id.to_owned(), workers.suspend(id).await?);
        }
        Ok(())
    }

    async fn resume(&self, workers: &SessionWorkers, id: &str) -> Result<()> {
        let mut suspended = self.0.lock().await;
        if let Some(guard) = suspended.remove(id) {
            guard.resume().await?;
        }
        workers.retry(id).await
    }
}

struct WorkerRef(RemoteReference);

impl Drop for WorkerRef {
    fn drop(&mut self) {
        self.0.release();
    }
}

struct Preparing {
    ids: watch::Sender<BTreeSet<String>>,
    copy: String,
}

impl Drop for Preparing {
    fn drop(&mut self) {
        self.ids.send_modify(|ids| {
            ids.remove(&self.copy);
        });
    }
}

async fn prepare(
    boundary: &WorkerRef,
    args: Value,
    row: &Value,
    verb: &str,
    context: &Context,
) -> Result<Option<String>> {
    let result: Value = boundary
        .0
        .call("prepare", wire(args), context)
        .await?
        .to_serde()?;
    if result["accepted"] == true {
        return Ok(None);
    }
    if let Some(refusal) = result["refusal"].as_str() {
        return Ok(Some(refusal.to_owned()));
    }
    let busy: Vec<Value> = serde_json::from_value(result["busy"].clone())?;
    if busy.contains(&row["main"]) {
        return Ok(Some(format!("Stop the current run before {verb}")));
    }
    Ok(Some(format!(
        "Background work is still running: wait for it to finish before {verb}"
    )))
}

async fn local_directory(args: &Value, absolute: bool, create: bool) -> Result<PathBuf> {
    if let Some(env) = args["env"].as_str() {
        return Err(Error::Invalid(format!(
            "Environment {env:?} is not configured; refusing local execution"
        )));
    }
    let cwd = arg(args, "cwd")?;
    let path = if cwd == "~" || cwd.starts_with("~/") {
        let home = std::env::home_dir()
            .ok_or_else(|| Error::Invalid("The home directory is unavailable".into()))?;
        home.join(cwd.strip_prefix("~/").unwrap_or_default())
    } else {
        PathBuf::from(cwd)
    };
    if absolute && !path.is_absolute() {
        return Err(Error::Invalid(
            "The working directory must be an absolute path".into(),
        ));
    }
    let path = std::path::absolute(path)?;
    if create {
        tokio::fs::create_dir_all(&path).await?;
    }
    if !tokio::fs::metadata(&path).await?.is_dir() {
        return Err(Error::Invalid(
            "Working directory is not a directory".into(),
        ));
    }
    Ok(path)
}
