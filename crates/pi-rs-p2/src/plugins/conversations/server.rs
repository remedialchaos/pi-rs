use std::sync::Arc;

use pi_rs_chord::{
    Error, Result,
    contracts::Contract,
    data::ChordError,
    host::{Ctx, Facet, Held, Members, ServeOptions, WeakHost, define_facet},
    json::JsonValue,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::api::v1::{Principal, p2_contracts::lookup, server::server_of};

#[derive(Clone)]
struct Broker {
    host: WeakHost,
    catalog: Arc<Contract>,
    status: Arc<Contract>,
    projects: Arc<Contract>,
    lifecycle: Arc<Contract>,
    models: Arc<Contract>,
    peer: Arc<Contract>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Request {
    source: String,
    request_id: String,
    query: Value,
}

pub(super) fn facet() -> Result<Facet> {
    let broker = Arc::new(lookup("p2.conversation-broker")?);
    let public = Arc::new(lookup("p2.conversations")?);
    let catalog = Arc::new(lookup("p2.catalog")?);
    let status = Arc::new(lookup("p2.status-store")?);
    let projects = Arc::new(lookup("p2.project-service")?);
    let lifecycle = Arc::new(lookup("p2.session-operations")?);
    let models = Arc::new(lookup("p2.model-config")?);
    let peer = Arc::new(lookup("p2.peer-conversation")?);
    Ok(define_facet(move |host| {
        let services = Broker {
            host: host.host().downgrade(),
            catalog: catalog.clone(),
            status: status.clone(),
            projects: projects.clone(),
            lifecycle: lifecycle.clone(),
            models: models.clone(),
            peer: peer.clone(),
        };
        host.serve(
            broker.clone(),
            move |_, _, _| {
                let mut members = Members::new();
                let services = services.clone();
                members.method("query", move |args, ctx| {
                    let services = services.clone();
                    Box::pin(async move {
                        let request = serde_json::from_value(args.to_serde()?)?;
                        Ok(services.query(request, &ctx).await?.into())
                    })
                })?;
                Ok(members)
            },
            ServeOptions::default(),
        )?;
        let weak = host.host().downgrade();
        let broker = broker.clone();
        host.serve(
            public.clone(),
            move |_, principal, _| {
                let Some(Principal::Worker { session, generation }) =
                    principal.map(Principal::deserialize).transpose()?
                else {
                    return Err(forbidden("conversation messaging is for session workers"));
                };
                let weak = weak.clone();
                let broker = broker.clone();
                let mut methods = Members::variant("worker");
                methods.method("query", move |args, ctx| {
                    let (weak, broker, session) = (weak.clone(), broker.clone(), session.clone());
                    Box::pin(async move {
                        let host = weak.upgrade().ok_or(Error::Closed)?;
                        let current = server_of(&host)?.workers.observe();
                        if !current.borrow().get(&session).is_some_and(|state| {
                            state.running && state.generation == generation
                        }) {
                            return Err(forbidden("stale worker"));
                        }
                        let args: Value = args.to_serde()?;
                        let broker = host.get(broker, json!({}).into(), &ctx.context).await?;
                        broker.call(
                            "query",
                            json!({"source":session,"requestId":args["requestId"],"query":args["query"]}).into(),
                            &ctx.context,
                        ).await
                    })
                })?;
                Ok(methods)
            },
            ServeOptions::default(),
        )
    }))
}

impl Broker {
    async fn hold(&self, contract: &Arc<Contract>, params: Value, ctx: &Ctx) -> Result<Held> {
        let host = self.host.upgrade().ok_or(Error::Closed)?;
        host.get(contract.clone(), params.into(), &ctx.context)
            .await
    }

    async fn query(&self, request: Request, ctx: &Ctx) -> Result<Value> {
        let catalog = self.hold(&self.catalog, json!({}), ctx).await?;
        let source = request.source.as_str();
        let sender = row(&catalog, source, ctx).await?;
        if sender["archivedAt"] != Value::Null {
            return Err(forbidden("The sending session is archived"));
        }
        let action = str_field(&request.query, "action")?;
        if action == "list" {
            return self.list(&catalog, source, ctx).await;
        }
        if action == "create" {
            let target = self.create(&sender, &request, ctx).await?;
            let to = str_field(&target, "id")?;
            self.deliver(
                &catalog,
                source,
                to,
                "send",
                submission(&request, &sender),
                ctx,
            )
            .await?;
            return Ok(
                json!({"action":action,"to":to,"title":target["title"],"env":target["env"],"cwd":target["cwd"]}),
            );
        }
        let to = str_field(&request.query, "to")?;
        if action != "restore" && action != "answer" && to == source {
            return Err(Error::Invalid(if action == "send" {
                "Cannot send a message to yourself".into()
            } else {
                format!("Cannot {action} yourself")
            }));
        }
        let recipient = row(&catalog, to, ctx).await?;
        if action != "restore" && !recipient["archivedAt"].is_null() {
            return Err(Error::Invalid("The destination is archived".into()));
        }
        if matches!(action, "archive" | "restore") {
            self.hold(&self.lifecycle, json!({}), ctx)
                .await?
                .call(action, json!({"id":to}).into(), &ctx.context)
                .await?;
            return Ok(json!({"action":action,"to":to}));
        }
        let args = if action == "send" {
            if str_field(&request.query, "message")?.trim().is_empty() {
                return Err(Error::Invalid("send needs a message".into()));
            }
            submission(&request, &sender)
        } else {
            Value::Null
        };
        let mut reply: Map<String, Value> = serde_json::from_value(
            self.deliver(&catalog, source, to, action, args, ctx)
                .await?,
        )?;
        reply.insert("action".into(), json!(action));
        reply.insert("to".into(), json!(to));
        Ok(Value::Object(reply))
    }

    async fn list(&self, catalog: &Held, source: &str, ctx: &Ctx) -> Result<Value> {
        let statuses = self.hold(&self.status, json!({}), ctx).await?;
        let projects = self.hold(&self.projects, json!({}), ctx).await?;
        let rows: Vec<Value> = serde_json::from_value(state(catalog, "sessions")?)?;
        let statuses = state(&statuses, "statuses")?;
        let memberships = state(&projects, "sessions")?;
        let mut active = rows.iter().filter(|row| row["archivedAt"].is_null());
        let peers = active.by_ref().take(200).map(|row| {
            let id = str_field(row, "id")?;
            let status = &statuses[id];
            let kind = if status["running"] != true {
                "stopped"
            } else if status["busy"].as_array().is_some_and(|busy| busy.contains(&json!(1))) {
                "working"
            } else {
                "idle"
            };
            let model = format!(
                "{}/{}",
                str_field(&row["model"], "provider")?,
                str_field(&row["model"], "modelId")?,
            );
            let mut value = json!({"id":id,"title":row["title"],"model":model,"state":kind,"env":row["env"],"cwd":row["cwd"]});
            if let Some(project) = memberships.get(id) {
                value["project"] = project.clone();
            }
            Ok(value)
        }).collect::<Result<Vec<_>>>()?;
        Ok(json!({
            "action":"list","self":source,"peers":peers,
            "truncated":active.next().is_some(),"projects":state(&projects, "projects")?,
        }))
    }

    async fn deliver(
        &self,
        catalog: &Held,
        source: &str,
        to: &str,
        method: &str,
        args: Value,
        ctx: &Ctx,
    ) -> Result<Value> {
        let attempt = async || {
            let peer = self.hold(&self.peer, json!({"session":to}), ctx).await?;
            if archived(catalog, source, to, ctx).await? {
                return Err(forbidden("Conversation is archived"));
            }
            peer.call(method, args.clone().into(), &ctx.context)
                .await?
                .to_serde()
        };
        let result = match attempt().await {
            Err(Error::Chord(error))
                if matches!(error.code.as_str(), "unavailable" | "gone")
                    && ctx.context.check().is_ok() =>
            {
                attempt().await?
            }
            result => result?,
        };
        if archived(catalog, source, to, ctx).await? {
            return Err(forbidden(
                "Conversation is archived; the action may already have taken effect",
            ));
        }
        Ok(result)
    }

    async fn create(&self, sender: &Value, request: &Request, ctx: &Ctx) -> Result<Value> {
        let query = &request.query;
        let project = query["project"].as_str();
        let env = query["env"].as_str();
        if project.is_some() && query["cwd"].is_string() {
            return Err(Error::Invalid(
                "create takes either a project or a cwd, not both".into(),
            ));
        }
        let projects = if project.is_some() {
            Some(self.hold(&self.projects, json!({}), ctx).await?)
        } else {
            None
        };
        let cwd = match (project, &projects) {
            (Some(name), Some(projects)) => place_of(&state(projects, "projects")?, name, env)?,
            _ => query["cwd"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| Error::Invalid("create needs either a project or a cwd".into()))?,
        };
        let id = derived_id(&request.source, &request.request_id);
        let mut args = json!({"id":id,"env":env,"cwd":cwd});
        if let Some(title) = query.get("title") {
            args["title"] = title.clone();
        }
        if let Some(named) = query["model"].as_str() {
            let (provider, model) = named
                .split_once('/')
                .filter(|(provider, _)| !provider.is_empty())
                .ok_or_else(|| Error::Invalid("model must be 'provider/modelId'".into()))?;
            let config = self.hold(&self.models, json!({}), ctx).await?;
            let summary = state(&config, "summary")?;
            if !summary["models"].as_array().is_some_and(|models| {
                models
                    .iter()
                    .any(|m| m["provider"] == provider && m["modelId"] == model)
            }) {
                return Err(Error::Invalid(format!(
                    "{named} is not a signed-in model; leave model out to use yours"
                )));
            }
            args["model"] = json!({"provider":provider,"modelId":model});
        } else {
            args["model"] = sender["model"].clone();
            if let Some(level) = sender["thinkingLevel"].as_str().filter(|level| {
                matches!(
                    *level,
                    "off" | "minimal" | "low" | "medium" | "high" | "xhigh"
                )
            }) {
                args["thinkingLevel"] = json!(level);
            }
        }
        let operations = self.hold(&self.lifecycle, json!({}), ctx).await?;
        let target: Value = operations
            .call("create", args.into(), &ctx.context)
            .await?
            .to_serde()?;
        if let (Some(name), Some(projects)) = (project, projects) {
            projects
                .call(
                    "assign",
                    json!({"session":target["id"],"project":name}).into(),
                    &ctx.context,
                )
                .await?;
        }
        Ok(target)
    }
}

fn submission(request: &Request, sender: &Value) -> Value {
    let mut args = json!({"source":request.source,"requestId":request.request_id,"title":sender["title"],"message":request.query["message"]});
    if let Some(value) = request.query.get("followUp") {
        args["followUp"] = value.clone();
    }
    args
}

fn state(held: &Held, name: &str) -> Result<Value> {
    held.member(name)?.value()?.to_json().to_serde()
}

async fn row(catalog: &Held, id: &str, ctx: &Ctx) -> Result<Value> {
    catalog
        .call("get", JsonValue::from_serde(json!({"id":id})), &ctx.context)
        .await?
        .to_serde()
}

async fn archived(catalog: &Held, from: &str, to: &str, ctx: &Ctx) -> Result<bool> {
    Ok(!(row(catalog, from, ctx).await?["archivedAt"].is_null()
        && row(catalog, to, ctx).await?["archivedAt"].is_null()))
}

fn place_of(projects: &Value, name: &str, env: Option<&str>) -> Result<String> {
    let projects = projects
        .as_array()
        .ok_or_else(|| Error::Invalid("Expected projects".into()))?;
    let project = projects
        .iter()
        .find(|p| p["name"] == name)
        .ok_or_else(|| Error::Invalid(format!("No project \"{name}\"; list shows the projects")))?;
    let places = project["places"]
        .as_array()
        .ok_or_else(|| Error::Invalid("Expected project places".into()))?;
    let machine = env.unwrap_or("local");
    let local = machine == "local" || hostname_matches(machine);
    let place = places
        .iter()
        .find(|place| place["env"] == machine)
        .or_else(|| {
            local
                .then(|| places.iter().find(|place| place["env"].is_null()))
                .flatten()
        })
        .ok_or_else(|| {
            Error::Invalid(format!(
                "Project \"{name}\" has no place on {}",
                env.unwrap_or("this device")
            ))
        })?;
    Ok(str_field(place, "path")?.into())
}

fn hostname_matches(machine: &str) -> bool {
    #[cfg(unix)]
    {
        nix::unistd::gethostname()
            .ok()
            .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(machine))
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMPUTERNAME")
            .ok()
            .is_some_and(|name| name.eq_ignore_ascii_case(machine))
    }
}

fn derived_id(source: &str, request: &str) -> String {
    let digest = Sha256::digest(format!("{source}:{request}"));
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest[..16]);
    uuid::Uuid::from_bytes(bytes).to_string()
}

fn str_field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| Error::Invalid(format!("Expected {key}")))
}

fn forbidden(message: &str) -> Error {
    ChordError::new("forbidden", message).into()
}
