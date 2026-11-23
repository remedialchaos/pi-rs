use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use pi_rs_chord::{
    Error, Result,
    contracts::Contract,
    data::ChordError,
    delta::Revision,
    host::{Ctx, Facet, Held, Members, ObjectSelf, ServeOptions, WeakHost, define_facet},
    json::JsonValue,
    state::Source,
    sync::SyncFuture,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    api::v1::{
        Principal,
        p2_contracts::lookup,
        server::install_service,
        worker::{WorkerProcess, worker_of},
    },
    runtime::main_conversation::main_conversation,
};

use super::{conversation, images, shell};

pub type SendRouter = Arc<dyn Fn(Value, Ctx) -> SyncFuture<Option<u64>> + Send + Sync>;

#[derive(Default)]
pub struct SendRouters(Mutex<BTreeMap<String, SendRouter>>);

impl SendRouters {
    pub fn insert(&self, name: String, route: SendRouter) -> Result<()> {
        let mut routes = self.0.lock().unwrap_or_else(|err| err.into_inner());
        match routes.entry(name) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(route);
                Ok(())
            }
            std::collections::btree_map::Entry::Occupied(entry) => Err(Error::Invalid(format!(
                "Duplicate chat send router {}",
                entry.key()
            ))),
        }
    }

    pub fn remove(&self, name: &str) {
        self.0
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .remove(name);
    }
}

#[derive(Clone)]
struct Chat {
    conversation: Held,
    shells: Held,
    live_shell: Source,
    worker: Arc<WorkerProcess>,
    host: WeakHost,
    id: u64,
    principal: Principal,
    routers: Arc<SendRouters>,
}

pub(super) fn worker() -> Result<Facet> {
    let conversation = Arc::new(lookup("p2.conversation")?);
    let chat = Arc::new(lookup("p2.chat")?);
    let costs = Arc::new(lookup("p2.chat-costs")?);
    let control = Arc::new(lookup("p2.run-control")?);
    let shells = Arc::new(lookup("p2.shell-commands")?);
    Ok(define_facet(move |host| {
        images::worker(host)?;
        let worker = worker_of(&host.host())?;
        let routers = Arc::new(SendRouters::default());
        install_service(&host.host(), "p2.chat-send-routers", &routers)?;
        shell::serve(host, shells.clone(), worker.clone())?;
        let internal_worker = worker.clone();
        host.serve(
            conversation.clone(),
            move |params, _, object| {
                let id = conversation::id_at(&params.to_serde()?, "conversation")?;
                conversation::members(object, internal_worker.clone(), id)
            },
            ServeOptions {
                linger: Duration::from_secs(30),
                ..Default::default()
            },
        )?;
        let (public_worker, contract, shells) =
            (worker.clone(), conversation.clone(), shells.clone());
        let weak = host.host().downgrade();
        host.serve(
            chat.clone(),
            move |params, principal, object| {
                let params: Value = params.to_serde()?;
                let principal = permit(&public_worker, principal, &params, false)?;
                let participant = principal.can_send(conversation::text(&params, "session")?);
                let id = conversation::id_at(&params, "conversation")?;
                let conversation = hold_conversation(object, contract.clone(), id)?;
                let shells = object.get(shells.clone(), json!({}).into())?;
                let live_shell = Source::derive(
                    object.batch(),
                    vec![shells.member("shells")?],
                    move |inputs| {
                        let states: Value = inputs[0].to_json().to_serde()?;
                        Ok(Revision::from_json(states[&id.to_string()].clone().into())?)
                    },
                    |_| {},
                );
                let mut members =
                    Members::variant(if participant { "participant" } else { "viewer" });
                for name in ["view", "stats", "model"] {
                    members.state(name, conversation.member(name)?)?;
                }
                members.state("liveShell", live_shell.clone())?;
                forward(&mut members, "entries", conversation.clone())?;
                if participant {
                    let chat = Chat {
                        conversation,
                        shells,
                        live_shell,
                        id,
                        principal,
                        worker: public_worker.clone(),
                        host: weak.clone(),
                        routers: routers.clone(),
                    };
                    for method in [
                        "send",
                        "stop",
                        "dequeue",
                        "configure",
                        "compact",
                        "abortCompaction",
                        "reset",
                        "dequeueOwn",
                        "shell",
                        "abortShell",
                    ] {
                        let chat = chat.clone();
                        members.method(method, move |args, ctx| {
                            let chat = chat.clone();
                            Box::pin(async move {
                                chat.worker
                                    .call(async {
                                        Ok(chat.call(method, args.to_serde()?, &ctx).await?.into())
                                    })
                                    .await
                            })
                        })?;
                    }
                }
                Ok(members)
            },
            ServeOptions::default(),
        )?;
        let cost_contract = conversation.clone();
        let cost_worker = worker.clone();
        host.serve(
            costs.clone(),
            move |params, principal, object| {
                let params = params.to_serde()?;
                permit(&cost_worker, principal, &params, false)?;
                let id = conversation::id_at(&params, "conversation")?;
                let mut members = Members::variant("viewer");
                forward(
                    &mut members,
                    "costs",
                    hold_conversation(object, cost_contract.clone(), id)?,
                )?;
                Ok(members)
            },
            ServeOptions::default(),
        )?;
        let contract = conversation.clone();
        host.serve(
            control.clone(),
            move |params, principal, object| {
                let params = params.to_serde()?;
                permit(&worker, principal, &params, true)?;
                let id = conversation::id_at(&params, "conversation")?;
                let conversation = hold_conversation(object, contract.clone(), id)?;
                let worker = worker.clone();
                let mut members = Members::variant("participant");
                members.method("stop", move |_, ctx| {
                    let (worker, conversation) = (worker.clone(), conversation.clone());
                    Box::pin(async move {
                        worker
                            .call(async {
                                let result: Value = conversation
                                    .call("stop", JsonValue::Null, &ctx.context)
                                    .await?
                                    .to_serde()?;
                                file_drafts(id, result["restored"].clone(), &ctx).await?;
                                Ok(JsonValue::Null)
                            })
                            .await
                    })
                })?;
                Ok(members)
            },
            ServeOptions::default(),
        )
    }))
}

impl Chat {
    async fn call(&self, method: &str, mut args: Value, ctx: &Ctx) -> Result<Value> {
        if matches!(method, "reset" | "configure") && !self.principal.is_owner() {
            return Err(forbidden(if method == "reset" {
                "only the owner starts a new context"
            } else {
                "only the owner configures the model"
            }));
        }
        if !matches!(method, "stop" | "abortShell" | "abortCompaction")
            && let Some(reason) = self
                .worker
                .registries
                .read_only_reason(self.id, &ctx.context)
                .await?
        {
            return Err(forbidden(&reason));
        }
        let author = self.author();
        let author_id = conversation::text(&author, "id")?;
        match method {
            "send" => self.send(args, author, ctx).await,
            "shell" => {
                if conversation::text(&args, "command")?.trim().is_empty() {
                    return Err(Error::Invalid("Invalid command".into()));
                }
                self.worker.registries.wait_for_input(&ctx.context).await?;
                args["author"] = author;
                args["conversation"] = json!(self.id);
                self.shells
                    .call("start", args.into(), &ctx.context)
                    .await?
                    .to_serde()
            }
            "abortShell" => {
                let shell: Value = self.live_shell.value()?.to_json().to_serde()?;
                if shell["id"] != args["shellId"] {
                    return Ok(json!({"aborted":false}));
                }
                if !self.principal.is_owner() && shell["author"]["id"] != author_id {
                    return Err(forbidden("only its author or the owner cancels a command"));
                }
                self.shells
                    .call("abort", args.into(), &ctx.context)
                    .await?
                    .to_serde()
            }
            "dequeueOwn" => {
                let result = self
                    .internal(method, json!({"authorId":author_id}), ctx)
                    .await?;
                let restored = restored(&result)
                    .map(|item| item["text"].clone())
                    .collect::<Vec<_>>();
                Ok(json!({"restored":restored,"skipped":result["skipped"]}))
            }
            "stop" | "dequeue" => {
                if method == "dequeue" {
                    args["restore"] = json!(args["restore"].as_bool().unwrap_or(false));
                }
                let result = self.internal(method, args, ctx).await?;
                let (own, others): (Vec<_>, Vec<_>) = restored(&result)
                    .cloned()
                    .partition(|item| item["author"]["id"] == author_id);
                file_drafts(self.id, json!(others), ctx).await?;
                Ok(if method == "stop" {
                    json!({"restored":own.into_iter().map(|item| item["text"].clone()).collect::<Vec<_>>()})
                } else {
                    json!({"text":own.first().map(|item| &item["text"])})
                })
            }
            "configure" => {
                self.internal(method, args.clone(), ctx).await?;
                let harness = self.worker.harness()?;
                if args.as_object().is_some_and(|input| !input.is_empty())
                    && main_conversation(&harness.durable, &ctx.context)
                        .await
                        .map_err(conversation::invalid)?
                        == self.id
                {
                    let host = self.host.upgrade().ok_or(Error::Closed)?;
                    let sessions = host
                        .get(
                            Arc::new(lookup("p2.sessions")?),
                            json!({}).into(),
                            &ctx.context,
                        )
                        .await?;
                    sessions
                        .call("configureSelf", args.into(), &ctx.context)
                        .await?;
                }
                Ok(Value::Null)
            }
            _ => self.internal(method, args, ctx).await,
        }
    }

    async fn send(&self, mut args: Value, author: Value, ctx: &Ctx) -> Result<Value> {
        let attachments = args
            .get("attachments")
            .cloned()
            .unwrap_or_else(|| json!([]));
        let any_files = attachments
            .as_array()
            .is_some_and(|items| !items.is_empty());
        if !any_files && conversation::text(&args, "text")?.trim().is_empty() {
            return Err(Error::Invalid("Invalid content".into()));
        }
        args["author"] = author;
        let mut routed = args.clone();
        routed["attachments"] = attachments.clone();
        routed["conversation"] = json!(self.id);
        let routers = self
            .routers
            .0
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for route in routers {
            if let Some(id) = route(routed.clone(), ctx.clone()).await? {
                return Ok(json!({"submissionId":id}));
            }
        }
        if let Some(fields) = args.as_object_mut() {
            fields.shift_remove("attachments");
        }
        if any_files {
            let files = ctx.get(Arc::new(lookup("p2.attachments")?), json!({}).into())?;
            args["files"] = files
                .call(
                    "resolve",
                    json!({"ids":attachments,"conversation":self.id}).into(),
                    &ctx.context,
                )
                .await?
                .to_serde()?;
        }
        self.internal("submit", args, ctx).await
    }

    fn author(&self) -> Value {
        if let Principal::Guest { id, name, .. } = &self.principal {
            return json!({"kind":"guest","id":id,"name":name});
        }
        let settings = self.worker.registries.settings.borrow();
        let name = settings["ownerName"].as_str().unwrap_or("Owner");
        json!({"kind":"owner","id":"owner","name":name})
    }

    async fn internal(&self, method: &str, args: Value, ctx: &Ctx) -> Result<Value> {
        self.conversation
            .call(method, args.into(), &ctx.context)
            .await?
            .to_serde()
    }
}

fn hold_conversation(object: &mut ObjectSelf, contract: Arc<Contract>, id: u64) -> Result<Held> {
    object.get(contract, json!({"conversation":id}).into())
}

fn forward(members: &mut Members, name: &'static str, to: Held) -> Result<()> {
    members.method(name, move |args, ctx| {
        let to = to.clone();
        Box::pin(async move { to.call(name, args, &ctx.context).await })
    })
}

fn permit(
    worker: &WorkerProcess,
    principal: Option<&Value>,
    params: &Value,
    send: bool,
) -> Result<Principal> {
    let session = conversation::text(params, "session")?;
    if worker.init.session()?.id != session {
        return Err(forbidden("session does not belong to this worker"));
    }
    principal
        .map(Principal::deserialize)
        .transpose()?
        .filter(|principal| {
            if send {
                principal.can_send(session)
            } else {
                principal.can_view(session)
            }
        })
        .ok_or_else(|| {
            forbidden(if send {
                "participants only"
            } else {
                "viewers only"
            })
        })
}

fn restored(result: &Value) -> impl Iterator<Item = &Value> {
    result["restored"].as_array().into_iter().flatten()
}

async fn file_drafts(conversation: u64, items: Value, ctx: &Ctx) -> Result<()> {
    if items.as_array().is_some_and(|items| !items.is_empty()) {
        let drafts = ctx.get(Arc::new(lookup("p2.draft-store")?), json!({}).into())?;
        drafts
            .call(
                "add",
                json!({"conversation":conversation,"items":items}).into(),
                &ctx.context,
            )
            .await?;
    }
    Ok(())
}

fn forbidden(message: &str) -> Error {
    ChordError::new("forbidden", message).into()
}
