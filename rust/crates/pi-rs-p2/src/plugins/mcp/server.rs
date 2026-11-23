//! Server-side lease authority and the Chord views scoped to each admitted principal.

use std::{path::Path, sync::Arc, time::Duration};

use indexmap::{IndexMap, IndexSet};
use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::{AcceptsTable, Contract, ValueAdapter, ValueMapping, VersionMappings},
    data::ChordError,
    delta::{Revision, Segment},
    documents::Definition,
    host::{Ctx, Facet, Held, Members, ObjectDocument, ObjectSelf, ServeOptions, define_facet},
    state::Source,
};
use pi_rs_mcp::oauth::OAuthCallback;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::{
    api::v1::{
        Principal,
        p2_contracts::lookup,
        server::{server_of, service_of},
    },
    runtime::{
        location::{Location, location_text, same_location},
        session_lock::SessionLock,
    },
};

use super::{
    config::OwnerConfig,
    config_file::{OwnerConfigFile, POLL_INTERVAL},
    credentials::Credentials,
    instances::Instances,
    leases::{
        Lease, LeaseCaller, LeaseHeld, LeasePlace, LeasesSet, ProjectSource, held_lease,
        release_generation, set_locations,
    },
    placement::{OAuthServer, Placement, oauth_server, place},
    probe::LoopbackProbe,
    routes,
    sign_in::SignIns,
    transports::ConnectRemote,
};

const METHODS: [&str; 10] = [
    "setLocations",
    "releaseLeases",
    "call",
    "restart",
    "beginSignIn",
    "completeSignIn",
    "pasteSignIn",
    "cancelSignIn",
    "signOut",
    "probe",
];

#[derive(Deserialize)]
#[serde(untagged)]
enum ProjectFile {
    Text { location: Location, text: String },
    Error { location: Location, error: String },
}

struct Peers {
    catalog: Option<Source>,
    children: Option<Source>,
    envs: Option<Source>,
    locks: Option<Source>,
}

struct Manager {
    file: OwnerConfigFile,
    leases: ObjectDocument,
    placement_lock: Mutex<()>,
    memento: ObjectDocument,
    instances: Instances,
    sign_ins: Arc<SignIns>,
    probe: LoopbackProbe,
    peers: Peers,
    state: Source,
}

pub(super) fn facet() -> Result<Facet> {
    let service = Arc::new(lookup("p2.mcp.manager")?);
    let public = Arc::new(lookup("p2.mcp")?);
    let hosts = Arc::new(lookup("p2.hosts")?);
    let leases = Arc::new(Definition::new(
        "p2.mcp.leases",
        json!({"type":"object","required":["leases"],"properties":{"leases":{
            "type":"array","items":{"type":"object","required":["session","location","project","generation","incarnation"],
                "properties":{"session":{"type":"string"},"location":{"type":"object"},"project":{"type":"object"},
                    "generation":{"type":"integer"},"incarnation":{"type":"string"}}}
        }}}).into(),
        1,
        || Ok(json!({"leases":[]}).into()),
    )?);
    let memento = Arc::new(Definition::new(
        "p2.mcp.memento",
        json!({"type":"object","required":["servers","errors","instances"],"properties":{
            "servers":{"type":"array","items":{}},"errors":{"type":"array","items":{"type":"string"}},
            "instances":{"type":"array","items":{"type":"object"}}
        }}).into(),
        1,
        || Ok(json!({"servers":[],"errors":[],"instances":[]}).into()),
    )?);
    Ok(define_facet(move |host| {
        host.contribute_http(routes::routes(host.host().downgrade(), service.clone())?)?;
        let (leases, memento) = (leases.clone(), memento.clone());
        host.serve(
            service.clone(),
            move |_, _, object| manager_members(object, leases.clone(), memento.clone()),
            ServeOptions::default(),
        )?;
        let (service, hosts) = (service.clone(), hosts.clone());
        host.serve(
            public.clone(),
            move |_, principal, object| view(object, principal, &service, &hosts),
            ServeOptions {
                accepts: accepts(),
                linger: Duration::from_secs(30),
                ..Default::default()
            },
        )
    }))
}

fn manager_members(
    object: &mut ObjectSelf,
    leases: Arc<Definition>,
    memento: Arc<Definition>,
) -> Result<Members> {
    let memento = object.state(memento)?;
    let leases = object.state(leases)?;
    let mut peers = Peers {
        catalog: peer(object, "p2.catalog", "sessions")?,
        children: peer(object, "p2.child-store", "children")?,
        envs: peer(object, "p2.env-service", "configs")?,
        locks: peer(object, "p2.session-locks", "locks")?,
    };
    let state =
        object.writable(json!({"instances":[],"leases":[],"errors":[],"signIns":[]}).into())?;
    let host = object.host().downgrade();
    let publish = state.clone();
    let manager = object.open(
        move |ctx| {
            Box::pin(async move {
                let host = host.upgrade().ok_or(Error::Closed)?;
                // An optional peer can be installed by a separate facet before the manager opens.
                for (slot, name, member) in [
                    (&mut peers.catalog, "p2.catalog", "sessions"),
                    (&mut peers.children, "p2.child-store", "children"),
                    (&mut peers.envs, "p2.env-service", "configs"),
                    (&mut peers.locks, "p2.session-locks", "locks"),
                ] {
                    if slot.is_none()
                        && let Some(contract) = host.served_contract(name)
                    {
                        *slot = Some(ctx.get(contract, json!({}).into())?.member(member)?);
                    }
                }
                let natives = server_of(&host)?;
                let data = Path::new(&natives.info.data_dir);
                let previous: Value = memento.value()?.to_serde()?;
                let file = OwnerConfigFile::new(
                    data,
                    serde_json::from_value::<OwnerConfig>(previous.clone())?,
                );
                file.read().await;
                let sign_ins = Arc::new(SignIns::new(Credentials::new(data), "p2".into()));
                let auth = sign_ins.clone();
                let remote = service_of::<ConnectRemote>(&host, super::environments::SERVICE)
                    .map_or_else(
                        |_| super::environments::connector(&host),
                        |remote| (*remote).clone(),
                    );
                let instances = Instances::new(
                    remote,
                    Arc::new(move |server| auth.provider(server)),
                    serde_json::from_value(previous["instances"].clone())?,
                )?;
                let manager = Manager {
                    file,
                    leases,
                    placement_lock: Mutex::default(),
                    memento,
                    instances,
                    sign_ins,
                    probe: LoopbackProbe::new(),
                    peers,
                    state: publish,
                };
                manager.refresh(&ctx.context).await?;
                Ok(manager)
            })
        },
        |manager, _| {
            Box::pin(async move {
                manager.instances.close().await;
                manager.sign_ins.close().await;
                Ok(())
            })
        },
    );

    let live = manager.clone();
    object.on_live(move |ctx| {
        let live = live.clone();
        Box::pin(async move {
            let live = live.value()?;
            let mut instances = live.instances.subscribe();
            let mut flows = live.sign_ins.subscribe();
            let mut auth = live.sign_ins.credentials_changed();
            let mut poll = tokio::time::interval(POLL_INTERVAL);
            let mut inputs = tokio::time::interval(Duration::from_millis(250));
            let run = async {
                loop {
                    tokio::select! {
                        del = auth.recv() => if let Ok(server) = del { live.instances.reconnect(&server).await; },
                        _ = poll.tick() => { live.file.poll().await; },
                        _ = inputs.tick() => {},
                        _ = instances.changed() => {},
                        _ = flows.changed() => {},
                    }
                    live.refresh(&ctx.context).await?;
                }
            };
            ctx.context.await_with(run).await.map_err(|error| Error::Invalid(error.to_string()))?
        })
    });
    let mut members = Members::new();
    for key in ["instances", "leases", "errors", "signIns"] {
        members.state(
            key,
            derived(object, vec![state.clone()], move |values| {
                Ok(values[0][key].clone())
            }),
        )?;
    }
    for method in METHODS {
        let manager = manager.clone();
        members.method(method, move |args, ctx| {
            let manager = manager.clone();
            Box::pin(async move {
                Ok(manager
                    .value()?
                    .call(method, args.to_serde()?, &ctx.context)
                    .await?
                    .into())
            })
        })?;
    }
    Ok(members)
}

fn peer(object: &mut ObjectSelf, name: &str, member: &str) -> Result<Option<Source>> {
    object
        .host()
        .served_contract(name)
        .map(|contract| object.get(contract, json!({}).into())?.member(member))
        .transpose()
}

fn read(source: Option<&Source>, fallback: Value) -> Result<Value> {
    match source {
        Some(source) if source.ready() => source.value()?.to_json().to_serde(),
        _ => Ok(fallback),
    }
}

impl Manager {
    fn leases(&self) -> Result<Vec<Lease>> {
        let value: Value = self.leases.value()?.to_serde()?;
        Ok(serde_json::from_value(value["leases"].clone())?)
    }

    async fn set_leases(&self, leases: Vec<Lease>, ctx: &Context) -> Result<()> {
        let value = serde_json::to_value(&leases)?;
        self.leases
            .change(ctx, move |draft| {
                Box::pin(async move { draft.set(&[Segment::Key("leases".into())], value.into()) })
            })
            .await?;
        self.publish(&self.placement(&leases)?, ctx).await
    }

    fn placement(&self, leases: &[Lease]) -> Result<Placement> {
        let sessions = read(self.peers.catalog.as_ref(), json!([]))?;
        let children = read(self.peers.children.as_ref(), json!({}))?;
        let envs = serde_json::from_value(read(self.peers.envs.as_ref(), json!({}))?)?;
        let locks = serde_json::from_value::<IndexMap<String, SessionLock>>(read(
            self.peers.locks.as_ref(),
            json!({}),
        )?)?;
        let mut work = IndexMap::new();
        for row in entries(&sessions) {
            let Some(id) = row["id"].as_str() else {
                continue;
            };
            let mut envs = IndexSet::from([row["env"].as_str().map(str::to_owned)]);
            envs.extend(entries(&children[id]).filter_map(|child| {
                child["location"]["env"]
                    .as_str()
                    .map(|env| Some(env.to_owned()))
            }));
            work.insert(id.to_owned(), envs.into_iter().collect());
        }
        Ok(place(
            &self.file.state().config,
            leases,
            &envs,
            &work,
            &locks,
        ))
    }

    async fn refresh(&self, ctx: &Context) -> Result<()> {
        let _guard = self.placement_lock.lock().await;
        let leases = self.leases()?;
        let placement = self.placement(&leases)?;
        self.publish(&placement, ctx).await
    }

    async fn publish(&self, placement: &Placement, ctx: &Context) -> Result<()> {
        self.instances.reconcile(placement);
        let leases = placement.leases.iter().map(|placed| {
            let ids = placed.servers.values().collect::<IndexSet<_>>();
            json!({"session":placed.lease.session,"location":placed.lease.location,"instances":ids,"errors":placed.errors})
        }).collect::<Vec<_>>();
        let configured = self.file.state();
        let instances = self.instances.list();
        let remember = json!({"servers":configured.config.servers,"errors":configured.config.errors,"instances":instances});
        if self.memento.value()?.to_serde()? != remember {
            self.memento
                .change(ctx, move |draft| {
                    Box::pin(async move {
                        for field in ["servers", "errors", "instances"] {
                            draft.set(
                                &[Segment::Key(field.into())],
                                remember[field].clone().into(),
                            )?;
                        }
                        Ok(())
                    })
                })
                .await?;
        }
        let mut errors = configured.config.errors;
        if let Some(failure) = configured.failure {
            errors.push(format!("{failure}; keeping the last valid servers"));
        }
        let next = json!({"instances":instances,"leases":leases,"errors":errors,"signIns":self.sign_ins.list().await});
        if self.state.value()?.to_json().to_serde()? != next {
            self.state.set(next.into(), ctx)?;
        }
        Ok(())
    }

    async fn call(&self, method: &str, args: Value, ctx: &Context) -> Result<Value> {
        match method {
            "setLocations" => {
                let caller = caller(&args)?;
                let locations: Vec<Location> = serde_json::from_value(args["locations"].clone())?;
                let projects: Vec<ProjectFile> = match args.get("projects") {
                    Some(projects) => serde_json::from_value(projects.clone())?,
                    None => Vec::new(),
                };
                let places = locations
                    .into_iter()
                    .map(|location| {
                        let project = projects
                            .iter()
                            .find_map(|file| match file {
                                ProjectFile::Text {
                                    location: path,
                                    text,
                                } if same_location(path, &location) => {
                                    Some(ProjectSource::File { text: text.clone() })
                                }
                                ProjectFile::Error {
                                    location: path,
                                    error,
                                } if same_location(path, &location) => {
                                    Some(ProjectSource::Unreadable {
                                        error: error.clone(),
                                    })
                                }
                                _ => None,
                            })
                            .unwrap_or(ProjectSource::None);
                        LeasePlace { location, project }
                    })
                    .collect::<Vec<_>>();
                let _guard = self.placement_lock.lock().await;
                let next = match set_locations(&self.leases()?, &caller, &places) {
                    LeasesSet::Set { leases } => leases,
                    LeasesSet::Refused { reason } => return Err(forbidden(reason)),
                };
                self.set_leases(next, ctx).await?;
            }
            "releaseLeases" => {
                let _guard = self.placement_lock.lock().await;
                let next = release_generation(
                    &self.leases()?,
                    field(&args, "session")?,
                    generation(&args)?,
                );
                self.set_leases(next, ctx).await?;
            }
            "call" => {
                let caller = caller(&args)?;
                let location: Location = serde_json::from_value(args["location"].clone())?;
                let name = field(&args, "server")?;
                let id = {
                    let _guard = self.placement_lock.lock().await;
                    let leases = self.leases()?;
                    let lease = match held_lease(&leases, &caller, &location) {
                        LeaseHeld::Held { lease } => lease,
                        LeaseHeld::Stale => {
                            return Err(forbidden(
                                "Another worker of this session holds its MCP servers",
                            ));
                        }
                        LeaseHeld::None => {
                            return Err(Error::Invalid(format!(
                                "No MCP servers are leased at {}",
                                location_text(&location)
                            )));
                        }
                    };
                    let placement = self.placement(&leases)?;
                    let id = placement
                        .leases
                        .iter()
                        .find(|view| view.lease == lease)
                        .and_then(|view| view.servers.get(name))
                        .cloned()
                        .ok_or_else(|| {
                            Error::Invalid(format!(
                                "MCP server \"{name}\" is not configured at {}",
                                location_text(&location)
                            ))
                        })?;
                    self.instances.reconcile(&placement);
                    id
                };
                let result = self
                    .instances
                    .call(&id, field(&args, "tool")?, args["args"].clone(), ctx)
                    .await?;
                return Ok(serde_json::to_value(result)?);
            }
            "restart" => self.instances.restart(field(&args, "instance")?).await?,
            "beginSignIn" => {
                let server = self.oauth(field(&args, "server")?)?;
                let route = args["redirect"]["url"].as_str().map(str::to_owned);
                let result = self
                    .sign_ins
                    .begin(server, route)
                    .await
                    .map_err(sign_in_error)?;
                self.refresh(ctx).await?;
                return Ok(result);
            }
            "completeSignIn" => {
                let name = self
                    .sign_ins
                    .complete(
                        OAuthCallback {
                            state: field(&args, "state")?.into(),
                            code: args["code"].as_str().unwrap_or_default().into(),
                            iss: args["iss"].as_str().map(str::to_owned),
                        },
                        args["error"].as_str().map(str::to_owned).or_else(|| {
                            args["code"]
                                .is_null()
                                .then(|| "The authorization response has no code".into())
                        }),
                    )
                    .await;
                self.refresh(ctx).await?;
                return Ok(json!({"server":name?}));
            }
            "pasteSignIn" => {
                let result = self
                    .sign_ins
                    .paste(field(&args, "id")?, field(&args, "redirectUrl")?)
                    .await;
                self.refresh(ctx).await?;
                result?;
            }
            "cancelSignIn" => self.sign_ins.cancel(field(&args, "id")?).await,
            "signOut" => self
                .sign_ins
                .sign_out(&self.oauth(field(&args, "server")?)?)
                .await
                .map_err(sign_in_error)?,
            "probe" => {
                return Ok(
                    json!({"url":self.probe.url(field(&args, "origin")?).await.map_err(Error::Invalid)?}),
                );
            }
            _ => {
                return Err(Error::Invalid(format!(
                    "Unknown MCP manager method {method}"
                )));
            }
        }
        if !matches!(method, "setLocations" | "releaseLeases" | "pasteSignIn") {
            self.refresh(ctx).await?;
        }
        Ok(Value::Null)
    }

    fn oauth(&self, name: &str) -> Result<OAuthServer> {
        self.file
            .state()
            .config
            .servers
            .iter()
            .filter_map(oauth_server)
            .find(|server| server.name == name)
            .ok_or_else(|| {
                ChordError::new(
                    "not_found",
                    format!("No MCP server \"{name}\" signs in with OAuth"),
                )
                .into()
            })
    }
}

fn view(
    object: &mut ObjectSelf,
    principal: Option<&Value>,
    contract: &Arc<Contract>,
    hosts: &Arc<Contract>,
) -> Result<Members> {
    let principal = principal.map(Principal::deserialize).transpose()?;
    if !matches!(
        principal,
        Some(Principal::Owner { .. } | Principal::Worker { .. })
    ) {
        return Err(forbidden(
            "MCP servers are for the owner and session workers",
        ));
    }
    let manager = object.get(contract.clone(), json!({}).into())?;
    if let Some(Principal::Worker {
        session,
        generation,
    }) = principal
    {
        let hosts = object
            .get(hosts.clone(), json!({}).into())?
            .member("workers")?;
        return worker_view(object, &manager, hosts, session, generation);
    }
    let mut members = Members::variant("owner");
    for key in ["instances", "signIns"] {
        members.state(key, manager.member(key)?)?;
    }
    members.state(
        "errors",
        derived(
            object,
            vec![manager.member("errors")?, manager.member("leases")?],
            |values| {
                let mut errors = entries(&values[0]).cloned().collect::<Vec<_>>();
                for lease in entries(&values[1]) {
                    errors.extend(entries(&lease["errors"]).filter_map(|error| {
                        Some(json!(format!(
                            "session {}: {}",
                            lease["session"].as_str()?,
                            error.as_str()?
                        )))
                    }));
                }
                Ok(json!(errors))
            },
        ),
    )?;
    for (method, target) in [
        ("restart", "restart"),
        ("signIn", "beginSignIn"),
        ("paste", "pasteSignIn"),
        ("cancelSignIn", "cancelSignIn"),
        ("signOut", "signOut"),
    ] {
        let manager = manager.clone();
        members.method(method, move |args, ctx| {
            let manager = manager.clone();
            Box::pin(async move {
                let args = if method == "signIn" {
                    let mut args: Value = args.to_serde()?;
                    args["redirect"] = json!({"kind":"loopback"});
                    args.into()
                } else {
                    args
                };
                manager.call(target, args, &ctx.context).await
            })
        })?;
    }
    Ok(members)
}

fn worker_view(
    object: &mut ObjectSelf,
    manager: &Held,
    hosts: Source,
    session: String,
    generation: u64,
) -> Result<Members> {
    let mut members = Members::variant("worker");
    for key in ["servers", "leases", "errors"] {
        let session = session.clone();
        members.state(key, derived(object, vec![manager.member("instances")?, manager.member("leases")?], move |values| {
            let leases = entries(&values[1]).filter(|lease| lease["session"] == session);
            let visible: Vec<Value> = match key {
                "servers" => {
                    let used = leases.flat_map(|lease| entries(&lease["instances"])).filter_map(Value::as_str).collect::<IndexSet<_>>();
                    entries(&values[0]).filter(|instance| instance["id"].as_str().is_some_and(|id| used.contains(id)))
                        .cloned().map(|mut instance| { if let Some(fields) = instance.as_object_mut() { fields.shift_remove("error"); } instance }).collect()
                }
                "leases" => leases.map(|lease| json!({"location":lease["location"],"instances":lease["instances"]})).collect(),
                _ => leases.flat_map(|lease| entries(&lease["errors"])).cloned().collect(),
            };
            Ok(json!(visible))
        }))?;
    }
    for method in ["setLocations", "call"] {
        let (manager, hosts, session) = (manager.clone(), hosts.clone(), session.clone());
        members.method(method, move |args, ctx| {
            let (manager, hosts, session) = (manager.clone(), hosts.clone(), session.clone());
            Box::pin(async move {
                current(&hosts, &session, generation, &ctx).await?;
                let mut args: Value = args.to_serde()?;
                args["session"] = json!(session);
                args["generation"] = json!(generation);
                args["incarnation"] = json!(ctx.incarnation);
                manager.call(method, args.into(), &ctx.context).await
            })
        })?;
    }
    let manager = manager.clone();
    object.open(
        |_| Box::pin(async { Ok(()) }),
        move |_, reason| {
            Box::pin(async move {
                if reason != "restart" && reason != "activation failed" && reason != "host closed" {
                    manager
                        .call(
                            "releaseLeases",
                            json!({"session":session,"generation":generation}).into(),
                            &Context::background(),
                        )
                        .await?;
                }
                Ok(())
            })
        },
    );
    Ok(members)
}

async fn current(hosts: &Source, session: &str, generation: u64, ctx: &Ctx) -> Result<()> {
    let (changed, mut updates) = tokio::sync::mpsc::channel(1);
    let stopped = changed.clone();
    let _subscription = hosts.subscribe(
        move |_, _| {
            let _ = changed.try_send(true);
            Ok(())
        },
        move |_| {
            let _ = stopped.try_send(false);
        },
    );
    loop {
        let workers = read(Some(hosts), json!({}))?;
        let state = &workers[session];
        match state["generation"].as_u64() {
            Some(current) if current > generation => return Err(forbidden("stale worker")),
            Some(current) if current == generation && state["running"] == true => return Ok(()),
            _ => {}
        }
        if ctx
            .context
            .await_with(updates.recv())
            .await
            .map_err(|error| Error::Invalid(error.to_string()))?
            != Some(true)
        {
            return Err(Error::Closed);
        }
    }
}

fn derived(
    object: &ObjectSelf,
    sources: Vec<Source>,
    read: impl Fn(Vec<Value>) -> Result<Value> + Send + Sync + 'static,
) -> Source {
    Source::derive(
        object.batch(),
        sources,
        move |values| {
            let values = values
                .iter()
                .map(|value| value.to_json().to_serde())
                .collect::<Result<Vec<_>>>()?;
            Ok(Revision::from_json(read(values)?.into())?)
        },
        |_| {},
    )
}

fn accepts() -> AcceptsTable {
    (2..=3)
        .map(|version| {
            let adapter: ValueAdapter = Arc::new(move |value| {
                let pi_rs_schema::Value::Array(mut instances) = value else {
                    return Err(Error::Invalid("Expected MCP instances".into()));
                };
                for instance in &mut instances {
                    if let pi_rs_schema::Value::Object(fields) = instance {
                        fields.shift_remove("description");
                        if version == 2 {
                            fields.shift_remove("instructions");
                        }
                    }
                }
                Ok(pi_rs_schema::Value::Array(instances))
            });
            (
                version,
                VersionMappings {
                    state: IndexMap::from([
                        (
                            ("worker".into(), "servers".into()),
                            ValueMapping::Map(adapter.clone()),
                        ),
                        (
                            ("owner".into(), "instances".into()),
                            ValueMapping::Map(adapter),
                        ),
                    ]),
                    ..Default::default()
                },
            )
        })
        .collect()
}

fn entries(value: &Value) -> impl Iterator<Item = &Value> {
    value.as_array().into_iter().flatten()
}
fn field<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value[field]
        .as_str()
        .ok_or_else(|| Error::Invalid(format!("Missing {field}")))
}
fn generation(args: &Value) -> Result<u64> {
    args["generation"]
        .as_u64()
        .ok_or_else(|| Error::Invalid("Invalid worker generation".into()))
}
fn caller(args: &Value) -> Result<LeaseCaller> {
    Ok(LeaseCaller {
        session: field(args, "session")?.into(),
        incarnation: field(args, "incarnation")?.into(),
        generation: generation(args)?,
    })
}
fn forbidden(message: impl Into<String>) -> Error {
    ChordError::new("forbidden", message).into()
}
fn sign_in_error(error: Error) -> Error {
    match error {
        Error::Chord(_) => error,
        _ => ChordError::new("sign_in", error.to_string()).into(),
    }
}
