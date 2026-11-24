//! The `ports` plugin's server facet: the `ForwardStore` the server keeps, and `p2.ports`
//! for the owner's Ports sheet and a session worker's `ports` tool.
//!
//! Upstream also contributes a `SessionArchived` cleanup (the sessions plugin's keyed native
//! registry) so a session's forwards close the moment it is archived, next to the sweep of
//! archived sessions the store runs at open. The native keyed-registry seam is owned by the
//! server-registry runtime; until it lands, the open-time sweep covers archived sessions.

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use pi_rs_chord::{
    Error, Result,
    contracts::Contract,
    delta::Revision,
    host::{Ctx, Facet, Held, Members, ObjectSelf, ServeOptions, define_facet},
    json::JsonValue,
    state::{NativeSource, Source},
};
use serde_json::{Value, json};

use crate::api::v1::{FacetKind, PluginRegistry, Principal, p2_contracts::lookup};

use super::{protocol, store::Forwards};

/// The open forwards, shared by the `ForwardStore` object and the `p2.ports` facets.
type OpenForwards = Arc<Mutex<Option<Arc<Forwards>>>>;

/// Upstream's description of the forwards native, in its not-open error.
const THE_FORWARDS: &str = "The forwards";

/// How long a call into another plugin's contract may take before the caller gives up.
const DEPENDENCY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

fn lock(cell: &OpenForwards) -> std::sync::MutexGuard<'_, Option<Arc<Forwards>>> {
    cell.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn forward_store_contract() -> Result<Arc<Contract>> {
    Ok(Arc::new(lookup("p2.forward-store")?))
}

/// The empty object parameters every local contract takes.
fn params() -> JsonValue {
    JsonValue::from_serde(json!({}))
}

fn params_of(value: Value) -> JsonValue {
    JsonValue::from_serde(value)
}

/// The shared store, as the `p2.ports` methods reach it: upstream's `nativeOf(ForwardsNative)`.
fn open_forwards(cell: &OpenForwards) -> Result<Arc<Forwards>> {
    lock(cell)
        .clone()
        .ok_or_else(|| pi_rs_chord::data::ChordError::new("not_ready", THE_FORWARDS).into())
}

/// A store list as the contract's `Forward` values.
fn forward_list(store: &Forwards) -> Result<Value> {
    let list: Vec<Value> = store
        .list()
        .iter()
        .map(serde_json::to_value)
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| Error::Invalid(e.to_string()))?;
    Ok(json!(list))
}

fn report(error: String) {
    eprintln!("p2 ports: the forwards state failed: {error}");
}

/// Registers the `ports` plugin's server facet.
pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    registry.register("ports", FacetKind::Server, server_facet()?)
}

/// One revision the store bumps after every change; the facet's state derives follow it.
type ForwardsRevision = Arc<NativeSource<u64>>;

pub fn server_facet() -> Result<Facet> {
    let store = forward_store_contract()?;
    let ports = Arc::new(lookup("p2.ports")?);
    let server = Arc::new(lookup("p2.server")?);
    let catalog = Arc::new(lookup("p2.catalog")?);
    let hosts = Arc::new(lookup("p2.hosts")?);
    Ok(define_facet(move |host| {
        // Created at load: the state derives of both objects follow it from their first read.
        let forwards: OpenForwards = Arc::default();
        let revision: ForwardsRevision =
            Arc::new(NativeSource::new(host.host().batch(), 0, |a, b| a == b));
        let changes = revision.project(
            |value| {
                Revision::from_json(JsonValue::from_serde(json!(value)))
                    .map_err(|e| Error::Invalid(e.to_string()))
            },
            report,
        )?;
        let store_for_store = store.clone();
        let (forwards_store, revision_store, server_store, catalog_store, changes_store) = (
            forwards.clone(),
            revision.clone(),
            server.clone(),
            catalog.clone(),
            changes.clone(),
        );
        host.serve(
            store_for_store,
            move |_, _, object| {
                forward_store_members(
                    object,
                    &forwards_store,
                    &revision_store,
                    &changes_store,
                    &server_store,
                    &catalog_store,
                )
            },
            ServeOptions::default(),
        )?;
        let (forwards_ports, server_ports, catalog_ports, hosts_ports) = (
            forwards.clone(),
            server.clone(),
            catalog.clone(),
            hosts.clone(),
        );
        host.serve(
            ports.clone(),
            move |_, principal, object| {
                ports_members(
                    object,
                    principal,
                    &forwards_ports,
                    &server_ports,
                    &catalog_ports,
                    &hosts_ports,
                )
            },
            ServeOptions::default(),
        )?;
        Ok(())
    }))
}

/// The store's list and Radius origin: a method's write resolves after the store bumped the
/// revision both derive from.
fn forward_store_members(
    object: &mut ObjectSelf,
    forwards: &OpenForwards,
    revision: &ForwardsRevision,
    changes: &Source,
    server: &Arc<Contract>,
    catalog: &Arc<Contract>,
) -> Result<Members> {
    // Recorded now, read once live: the server names the data directory.
    let server_held = object.get(server.clone(), params())?;
    let catalog_held = object.get(catalog.clone(), params())?;
    let host_handle = object.host();
    let resolver_slot: Arc<Mutex<Option<Arc<crate::plugins::radius::ResolveForward>>>> =
        Arc::default();
    let opened = object.open(
        {
            let forwards = forwards.clone();
            let revision = revision.clone();
            let resolver_slot = resolver_slot.clone();
            move |ctx: Ctx| {
                let server = server_held.clone();
                let catalog = catalog_held.clone();
                let forwards = forwards.clone();
                let revision = revision.clone();
                let host_handle = host_handle.clone();
                let resolver_slot = resolver_slot.clone();
                Box::pin(async move {
                    server.wait(&ctx.context).await?;
                    let info = server.read("info")?.to_serde()?;
                    let data_dir = info["dataDir"]
                        .as_str()
                        .ok_or_else(|| {
                            Error::Invalid("the server's data directory is unknown".into())
                        })?
                        .to_owned();
                    let store = Forwards::open(Path::new(&data_dir), (*revision).clone()).await?;
                    // Forwards of archived sessions close first, so the list never shows them.
                    catalog.wait(&ctx.context).await?;
                    if let Ok(sessions) = catalog.read("sessions")
                        && let Ok(sessions) = sessions.to_serde()
                        && let Some(rows) = sessions.as_array()
                    {
                        for session in rows {
                            if session["archivedAt"].as_f64().is_some()
                                && let Some(id) = session["id"].as_str()
                            {
                                store.close_session(id, &ctx.context).await?;
                            }
                        }
                    }
                    *lock(&forwards) = Some(store.clone());
                    let resolver_store = store.clone();
                    let resolver: Arc<crate::plugins::radius::ResolveForward> =
                        Arc::new(Arc::new(move |id: &str| {
                            resolver_store
                                .get(id)
                                .map(|forward| crate::plugins::radius::Forward {
                                    port: forward.port,
                                    public: forward.public.unwrap_or(false),
                                })
                        }));
                    let _ = crate::api::v1::server::install_service(
                        &host_handle,
                        crate::plugins::radius::FORWARD_RESOLVER,
                        &resolver,
                    );
                    *resolver_slot
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(resolver);
                    Ok(store)
                })
            }
        },
        {
            let forwards = forwards.clone();
            let resolver_slot = resolver_slot.clone();
            move |_, _| {
                let forwards = forwards.clone();
                let resolver_slot = resolver_slot.clone();
                Box::pin(async move {
                    lock(&forwards).take();
                    resolver_slot
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take();
                    Ok(())
                })
            }
        },
    );
    let batch = object.batch();
    let list = {
        let opened = opened.clone();
        let changes = changes.clone();
        Source::derive(
            batch.clone(),
            vec![changes],
            move |_| {
                let store = opened.value()?;
                let list = forward_list(&store)?;
                Revision::from_json(JsonValue::from_serde(list))
                    .map_err(|e| Error::Invalid(e.to_string()))
            },
            report,
        )
    };
    let radius = {
        let opened = opened.clone();
        let changes = changes.clone();
        Source::derive(
            batch,
            vec![changes],
            move |_| {
                let origin = json!(opened.value()?.radius_origin());
                Revision::from_json(JsonValue::from_serde(origin))
                    .map_err(|e| Error::Invalid(e.to_string()))
            },
            report,
        )
    };
    let mut members = Members::new();
    members.state("list", list)?;
    members.state("radiusOrigin", radius)?;
    Ok(members)
}

/// `p2.ports`: the owner's Ports sheet, or the session worker's `ports` tool.
fn ports_members(
    object: &mut ObjectSelf,
    principal: Option<&Value>,
    forwards: &OpenForwards,
    server: &Arc<Contract>,
    catalog: &Arc<Contract>,
    hosts: &Arc<Contract>,
) -> Result<Members> {
    let principal =
        principal.and_then(|value| serde_json::from_value::<Principal>(value.clone()).ok());
    match principal {
        Some(Principal::Owner { .. }) => {
            // The singletons this object reads are live long before its hold: their members
            // can be the derive's inputs, so no compute reads another object's source.
            let store = object.get(forward_store_contract()?, params())?;
            let catalog_held = object.get(catalog.clone(), params())?;
            let server_held = object.get(server.clone(), params())?;
            let list = store.member("list")?;
            let radius = store.member("radiusOrigin")?;
            let sessions = catalog_held.member("sessions")?;
            let batch = object.batch();
            let view = Source::derive(
                batch.clone(),
                vec![list, sessions, radius],
                move |values| {
                    let [list, sessions, origin] = values else {
                        return Err(Error::Invalid("the forwards inputs".into()));
                    };
                    let sessions = sessions.to_json().to_serde()?;
                    let origin = origin.to_json().to_serde()?.as_str().map(str::to_owned);
                    let views: Vec<Value> = list
                        .to_json()
                        .to_serde()?
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .iter()
                        .map(|forward| {
                            let title = sessions
                                .as_array()
                                .and_then(|rows| {
                                    rows.iter().find(|row| {
                                        row["id"].as_str().is_some_and(|id| {
                                            forward["session"].as_str() == Some(id)
                                        })
                                    })
                                })
                                .and_then(|row| row["title"].as_str());
                            match serde_json::from_value::<protocol::Forward>(forward.clone()) {
                                Ok(parsed) => {
                                    protocol::forward_view(&parsed, origin.as_deref(), title)
                                }
                                Err(_) => json!({}),
                            }
                        })
                        .collect();
                    Revision::from_json(JsonValue::from_serde(json!(views)))
                        .map_err(|e| Error::Invalid(e.to_string()))
                },
                report,
            );
            let mut members = Members::variant("owner");
            members.state("forwards", view)?;
            // The store's own Radius origin, as its state holds it.
            members.state("radiusOrigin", store.member("radiusOrigin")?)?;
            owner_methods(members, forwards, server_held)
        }
        Some(Principal::Worker {
            session,
            generation,
        }) => {
            let _store = object.get(forward_store_contract()?, params())?;
            let catalog_held = object.get(catalog.clone(), params())?;
            let workers_held = object.get(hosts.clone(), params())?;
            let server_held = object.get(server.clone(), params())?;
            let mut members = Members::variant("worker");
            let cell = forwards.clone();
            members.method("query", move |args, ctx| {
                let (cell, session, deps) = (
                    cell.clone(),
                    session.clone(),
                    WorkerDeps {
                        catalog: catalog_held.clone(),
                        workers: workers_held.clone(),
                        server: server_held.clone(),
                    },
                );
                Box::pin(async move {
                    worker_query(args, &ctx, &cell, &session, generation, &deps).await
                })
            })?;
            Ok(members)
        }
        _ => Err(pi_rs_chord::data::ChordError::new("forbidden", "ports are owner-only").into()),
    }
}

/// The owner's `add`, `remove` and `setPublic`, over the shared store.
fn owner_methods(mut members: Members, forwards: &OpenForwards, server: Held) -> Result<Members> {
    let add = (forwards.clone(), server.clone());
    members.method("add", move |args, ctx| {
        let (forwards, server) = (add.0.clone(), add.1.clone());
        Box::pin(async move {
            let args = args.to_serde()?;
            let port = protocol::validate_forward_port(&args["port"])?;
            let reserved = [own_port(&server, &ctx).await?];
            open_forwards(&forwards)?
                .add(
                    port,
                    args.get("name"),
                    &reserved,
                    args["public"].as_bool().unwrap_or(false),
                    None,
                    &ctx.context,
                )
                .await?;
            Ok(JsonValue::Null)
        })
    })?;
    let remove = forwards.clone();
    members.method("remove", move |args, ctx| {
        let forwards = remove.clone();
        Box::pin(async move {
            let id = args.to_serde()?["id"]
                .as_str()
                .ok_or_else(|| Error::Invalid("a forward id is required".into()))?
                .to_owned();
            open_forwards(&forwards)?.remove(&id, &ctx.context).await?;
            Ok(JsonValue::Null)
        })
    })?;
    let set_public = forwards.clone();
    members.method("setPublic", move |args, ctx| {
        let forwards = set_public.clone();
        Box::pin(async move {
            let args = args.to_serde()?;
            let id = args["id"]
                .as_str()
                .ok_or_else(|| Error::Invalid("a forward id is required".into()))?;
            let open = args["public"].as_bool().unwrap_or(false);
            open_forwards(&forwards)?
                .set_public(id, open, &ctx.context)
                .await?;
            Ok(JsonValue::Null)
        })
    })?;
    Ok(members)
}

/// The server's own port, read from its contract once live.
async fn own_port(server: &Held, ctx: &Ctx) -> Result<u16> {
    server.wait(&ctx.context).await?;
    let info = server.read("info")?.to_serde()?;
    Ok(info["port"].as_u64().unwrap_or_default() as u16)
}

/// The contracts the worker's `query` reads, as one group.
#[derive(Clone)]
struct WorkerDeps {
    catalog: Held,
    workers: Held,
    server: Held,
}

/// The `ports` tool's request from this session's worker, as the server knows the session.
async fn worker_query(
    args: JsonValue,
    ctx: &Ctx,
    forwards: &OpenForwards,
    session: &str,
    generation: u64,
    deps: &WorkerDeps,
) -> Result<JsonValue> {
    // Only a current worker generation, for its own session.
    deps.workers.wait(&ctx.context).await?;
    let current = deps.workers.read("workers")?.to_serde()?;
    let worker = &current[session];
    if worker["running"].as_bool() != Some(true)
        || worker["generation"].as_u64() != Some(generation)
    {
        return Err(pi_rs_chord::data::ChordError::new("forbidden", "stale worker").into());
    }
    deps.catalog.wait(&ctx.context).await?;
    let live =
        call_dependency(ctx, &deps.catalog, "get", params_of(json!({"id": session}))).await?;
    let live = live.to_serde()?;
    if live["archivedAt"].as_f64().is_some() {
        return Err(pi_rs_chord::data::ChordError::new("forbidden", "Session is archived").into());
    }
    let query = args.to_serde()?["query"].clone();
    let action = query["action"].as_str().unwrap_or_default();
    if live["env"].as_str().is_some() && action == "open" {
        let env = live["env"].as_str().unwrap_or_default();
        return Err(Error::Invalid(format!(
            "This session runs on \"{env}\": ports of remote environments cannot be forwarded yet, only ports of this device"
        )));
    }
    let opened = open_forwards(forwards)?;
    let reserved = [own_port(&deps.server, ctx).await?];
    let mut reply = json!({"addressesKnown": opened.radius_origin().is_some()});
    if action == "open" {
        let port = protocol::validate_forward_port(&query["port"])?;
        let forward = opened
            .open_for_session(
                session,
                port,
                query.get("name"),
                query.get("public").and_then(Value::as_bool),
                &reserved,
                &ctx.context,
            )
            .await?;
        reply["forward"] = view_with_title(&forward, &opened, &deps.catalog)?;
    } else if action == "close" {
        let port = protocol::validate_forward_port(&query["port"])?;
        reply["closed"] = json!(opened.close_port(port, &ctx.context).await?);
    }
    let sessions = deps.catalog.read("sessions")?.to_serde()?;
    let views: Vec<Value> = opened
        .list()
        .iter()
        .map(|forward| {
            let title = sessions
                .as_array()
                .and_then(|rows| {
                    rows.iter().find(|row| {
                        row["id"]
                            .as_str()
                            .is_some_and(|id| forward.session.as_deref() == Some(id))
                    })
                })
                .and_then(|row| row["title"].as_str());
            protocol::forward_view(forward, opened.radius_origin().as_deref(), title)
        })
        .collect();
    reply["forwards"] = json!(views);
    Ok(JsonValue::from_serde(reply))
}

/// A call into another plugin's contract, bounded: an absent plugin must not hang the caller.
async fn call_dependency(
    ctx: &Ctx,
    held: &Held,
    method: &str,
    args: JsonValue,
) -> Result<JsonValue> {
    match tokio::time::timeout(DEPENDENCY_TIMEOUT, ctx.call(held, method, args)).await {
        Ok(result) => result,
        Err(_) => Err(Error::Invalid(format!(
            "The {method} call timed out; the plugin that serves it is not ready"
        ))),
    }
}

/// A forward's view with its opening session's title.
fn view_with_title(
    forward: &protocol::Forward,
    store: &Arc<Forwards>,
    catalog: &Held,
) -> Result<Value> {
    let sessions = catalog.read("sessions")?.to_serde()?;
    let title = sessions
        .as_array()
        .and_then(|rows| {
            rows.iter().find(|row| {
                row["id"]
                    .as_str()
                    .is_some_and(|id| forward.session.as_deref() == Some(id))
            })
        })
        .and_then(|row| row["title"].as_str());
    Ok(protocol::forward_view(
        forward,
        store.radius_origin().as_deref(),
        title,
    ))
}
