use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use pi_rs_chord::{
    Result,
    context::Context,
    data::ChordError,
    host::{Facet, Held, Members, ServeOptions, WeakHost, define_facet},
    json::JsonValue,
    node::http::{HttpContribution, HttpRequest, HttpResult, SocketOptions},
    watch::WatchCall,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::{
    api::v1::{Principal, p2_contracts::lookup},
    runtime::{server::server_of, session_worker::Location},
};

use super::service::{Service, Terminal, Theme, invalid, lock};

const WINDOW: u64 = 256 * 1024;

#[derive(Clone, Default)]
struct Attachment {
    terminal: Option<Arc<Terminal>>,
    epoch: u64,
    ack: u64,
}

struct View {
    session: String,
    attached: watch::Sender<Attachment>,
    theme: Mutex<Theme>,
    closed: watch::Sender<bool>,
    events: tokio::sync::broadcast::Sender<Value>,
}

impl View {
    fn new(session: String) -> Self {
        Self {
            session,
            attached: watch::channel(Attachment::default()).0,
            theme: Mutex::default(),
            closed: watch::channel(false).0,
            events: tokio::sync::broadcast::channel(64).0,
        }
    }

    async fn attach(&self, terminal: Option<Arc<Terminal>>) -> Result<()> {
        if let Some(terminal) = &terminal {
            let theme = *lock(&self.theme);
            terminal.theme(theme).await?;
        }
        self.attached.send_modify(|attached| {
            attached.terminal = terminal;
            attached.epoch = attached.epoch.wrapping_add(1);
            attached.ack = 0;
        });
        Ok(())
    }
}

type ChannelToken = (Weak<View>, Instant);

#[derive(Default)]
struct Channels(Mutex<HashMap<String, ChannelToken>>);

impl Channels {
    fn issue(&self, view: &Arc<View>) -> Value {
        let mut tokens = lock(&self.0);
        let weak = Arc::downgrade(view);
        let now = Instant::now();
        tokens.retain(|_, (other, expires)| !other.ptr_eq(&weak) && *expires > now);
        let mut random = [0_u8; 32];
        random[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        random[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        let token = URL_SAFE_NO_PAD.encode(&random[..24]);
        tokens.insert(token.clone(), (weak, now + Duration::from_secs(30)));
        json!({"token":token})
    }

    fn take(&self, token: &str) -> Option<Arc<View>> {
        let (view, expires) = lock(&self.0).remove(token)?;
        if expires <= Instant::now() {
            return None;
        }
        view.upgrade().filter(|view| !*view.closed.borrow())
    }
}

fn owner(principal: Option<Principal>) -> bool {
    matches!(principal, Some(Principal::Owner { .. }))
}

fn forbidden(message: &str) -> pi_rs_chord::Error {
    ChordError::new("forbidden", message).into()
}

async fn row(catalog: &Held, session: &str, ctx: &Context) -> Result<Value> {
    catalog
        .call("get", json!({"id":session}).into(), ctx)
        .await?
        .to_serde()
}

fn children(child_store: Option<&Held>, session: &str) -> Result<Vec<Value>> {
    let Some(store) = child_store else {
        return Ok(Vec::new());
    };
    let value = store.read("children")?.to_serde()?;
    Ok(value[session].as_array().cloned().unwrap_or_default())
}

fn location(session: &Value, child: Option<&Value>) -> Result<Location> {
    if let Some(child) = child.filter(|child| child["location"].is_object()) {
        return Ok(serde_json::from_value(child["location"].clone())?);
    }
    Ok(serde_json::from_value(
        json!({"cwd":session["cwd"],"env":session["env"]}),
    )?)
}

async fn current_worker(
    host: WeakHost,
    session: &str,
    generation: u64,
    ctx: &Context,
) -> Result<()> {
    let host = host.upgrade().ok_or(pi_rs_chord::Error::Closed)?;
    let mut workers = server_of(&host)?.workers.observe();
    ctx.await_with(workers.wait_for(|workers| {
        workers.get(session).is_some_and(|state| {
            state.generation > generation || (state.generation == generation && state.running)
        })
    }))
    .await
    .map_err(invalid)?
    .map_err(invalid)?;
    if workers
        .borrow()
        .get(session)
        .is_some_and(|state| state.running && state.generation == generation)
    {
        Ok(())
    } else {
        Err(forbidden("stale worker"))
    }
}

pub(super) fn facet() -> Result<Facet> {
    let service_contract = Arc::new(lookup("p2.terminal-service")?);
    let terminals = Arc::new(lookup("p2.terminals")?);
    let agent = Arc::new(lookup("p2.agent-terminals")?);
    let catalog = Arc::new(lookup("p2.catalog")?);
    let children_contract = Arc::new(lookup("p2.child-store")?);
    Ok(define_facet(move |host| {
        let service = Arc::new(Service::new(host.host().downgrade()));
        let channels = Arc::new(Channels::default());
        let mut http = HttpContribution::new("p2.terminals");
        let socket_channels = channels.clone();
        let socket_service = service.clone();
        http.sockets.insert(
            "/terminal".into(),
            SocketOptions {
                public: false,
                max_payload: 1024 * 1024,
                handler: Arc::new(move |socket, request| {
                    Box::pin(channel(
                        socket,
                        request,
                        socket_channels.clone(),
                        socket_service.clone(),
                    ))
                }),
            },
        );
        host.contribute_http(http)?;

        let srv = Arc::downgrade(&service);
        let cat = catalog.clone();
        host.serve(
            service_contract.clone(),
            move |_, _, object| {
                if let Some(service) = srv.upgrade() {
                    let rehydrate_service = service;
                    tokio::spawn(async move {
                        let _ = rehydrate_service.rehydrate().await;
                    });
                }
                let catalog = object.get(cat.clone(), json!({}).into())?;
                let service = srv.clone();
                object.follow(move |target, ctx| {
                    Box::pin(async move {
                        let source = catalog.member("sessions")?;
                        let close_archived = move |value: JsonValue| -> Result<()> {
                            if let Some(service) = service.upgrade()
                                && let Some(rows) = value.to_serde()?.as_array()
                            {
                                for row in rows {
                                    if !row["archivedAt"].is_null()
                                        && let Some(id) = row["id"].as_str()
                                    {
                                        service.close_session(id);
                                    }
                                }
                            }
                            Ok(())
                        };
                        close_archived(source.value()?.to_json())?;
                        target.set(JsonValue::Null, &ctx.context)?;
                        let subscription = source.subscribe(
                            move |call, _| {
                                if let WatchCall::Value { value, .. } = call {
                                    close_archived(value)?;
                                }
                                Ok(())
                            },
                            |_| {},
                        );
                        Ok(Box::new(move || drop(subscription)) as Box<dyn FnOnce() + Send>)
                    })
                })?;
                Ok(Members::new())
            },
            ServeOptions::default(),
        )?;

        let svc = service_contract.clone();
        let catalog_for_owner = catalog.clone();
        let children_for_owner = children_contract.clone();
        let owner_service = service.clone();
        host.serve(terminals.clone(), move |params, principal, object| {
            if !owner(principal.map(Principal::deserialize).transpose()?) {
                return Err(forbidden("terminals are owner-only"));
            }
            let session = params.to_serde()?["session"].as_str().ok_or_else(|| invalid("Invalid session"))?.to_owned();
            let _lease = object.get(svc.clone(),json!({}).into())?;
            let catalog = object.get(catalog_for_owner.clone(),json!({}).into())?;
            let child_store = object.host().served_contract("p2.child-store").map(|_| {
                object.get(children_for_owner.clone(),json!({}).into())
            }).transpose()?;
            let open_catalog = catalog.clone();
            let view = object.open(move |ctx| Box::pin(async move {
                row(&open_catalog,&session,&ctx.context).await?;
                Ok(View::new(session))
            }), |view,_| Box::pin(async move {
                view.closed.send_replace(true);
                Ok(())
            }));
            let mut members = Members::variant("owner");
            for method in ["channel","list","open","attach","close","resize","theme","ack","restartHost"] {
                let view = view.clone();
                let service = owner_service.clone();
                let channels = channels.clone();
                let catalog = catalog.clone();
                let child_store = child_store.clone();
                members.method(method,move |args,ctx| {
                    let (view,service,channels,catalog,child_store) = (view.clone(),service.clone(),channels.clone(),catalog.clone(),child_store.clone());
                    Box::pin(async move {
                        let view = view.value()?;
                        let args = args.to_serde()?;
                        let attached = view.attached.borrow().terminal.clone();
                        match method {
                            "channel" => return Ok(channels.issue(&view).into()),
                            "list" => { let _ = view.events.send(list_event(&service,&view.session,"list")); }
                            "open" => {
                                let row = row(&catalog,&view.session,&ctx.context).await?;
                                if !row["archivedAt"].is_null() { return Err(invalid("Session is archived")); }
                                let id = args["conversation"].as_u64().ok_or_else(|| invalid("Invalid conversation"))?;
                                let children = children(child_store.as_ref(),&view.session)?;
                                let child = children.iter().find(|child| child["conversationId"].as_u64() == Some(id));
                                if id != 1 && row["main"].as_u64() != Some(id) && child.is_none() {
                                    return Err(invalid("Unknown conversation"));
                                }
                                let terminal = service.open(view.session.clone(),location(&row,child)?,size(&args)?,None,false).await?;
                                view.attach(Some(terminal)).await?;
                            }
                            "attach" => {
                                let id = args["terminalId"].as_str().unwrap_or_default();
                                let terminal = service.list(&view.session).into_iter().find(|terminal| lock(&terminal.info).id == id)
                                    .ok_or_else(|| forbidden("This terminal does not belong to this session"))?;
                                view.attach(Some(terminal)).await?;
                            }
                            "close" => {
                                if let Some(terminal) = attached {
                                    if args["force"] != true && let Some(program) = terminal.foreground().await {
                                        let _ = view.events.send(json!({"type":"busy","terminalId":lock(&terminal.info).id,"program":program}));
                                    } else {
                                        let id = service.remove(&terminal);
                                        view.attach(None).await?;
                                        let _ = view.events.send(json!({"type":"closed","terminalId":id}));
                                    }
                                }
                            }
                            "resize" => {
                                if let Some(terminal) = attached {
                                    let (cols,rows) = size(&args)?;
                                    terminal.resize(cols,rows).await?;
                                    let _ = view.events.send(json!({"type":"changed","terminal":*lock(&terminal.info)}));
                                }
                            }
                            "ack" => view.attached.send_modify(|attached| {
                                attached.ack = attached.ack.saturating_add(args["bytes"].as_u64().unwrap_or_default());
                            }),
                            "restartHost" => {
                                service.close_all();
                                view.attach(None).await?;
                            }
                            "theme" => {
                                let theme = Theme {
                                    foreground: args["foreground"].as_u64().unwrap_or_default() as u32,
                                    background: args["background"].as_u64().unwrap_or_default() as u32,
                                    dark: args["scheme"] == "dark",
                                };
                                *lock(&view.theme) = theme;
                                if let Some(terminal) = attached {
                                    terminal.theme(theme).await?;
                                }
                            }
                            _ => return Err(invalid("Unknown terminal view method")),
                        }
                        Ok(JsonValue::Null)
                    })
                })?;
            }
            Ok(members)
        },ServeOptions::default())?;

        let svc = service_contract.clone();
        let catalog = catalog.clone();
        let children_contract = children_contract.clone();
        host.serve(
            agent.clone(),
            move |_, principal, object| {
                let Some(Principal::Worker {
                    session,
                    generation,
                }) = principal.map(Principal::deserialize).transpose()?
                else {
                    return Err(forbidden("agent terminals are for session workers"));
                };
                let _lease = object.get(svc.clone(), json!({}).into())?;
                let catalog = object.get(catalog.clone(), json!({}).into())?;
                let child_store = object
                    .host()
                    .served_contract("p2.child-store")
                    .map(|_| object.get(children_contract.clone(), json!({}).into()))
                    .transpose()?;
                let host = object.host().downgrade();
                let mut members = Members::variant("worker");
                for method in ["list", "open", "send", "cursor", "read", "capture", "close"] {
                    let (service, host, session, catalog, child_store) = (
                        service.clone(),
                        host.clone(),
                        session.clone(),
                        catalog.clone(),
                        child_store.clone(),
                    );
                    members.method(method, move |args, ctx| {
                        let (service, host, session, catalog, child_store) = (
                            service.clone(),
                            host.clone(),
                            session.clone(),
                            catalog.clone(),
                            child_store.clone(),
                        );
                        Box::pin(async move {
                            current_worker(host, &session, generation, &ctx.context).await?;
                            let row = row(&catalog, &session, &ctx.context).await?;
                            if !row["archivedAt"].is_null() {
                                return Err(forbidden("Session is archived"));
                            }
                            let args = args.to_serde()?;
                            if method == "open" && args["location"]["env"] != row["env"] {
                                let allowed = children(child_store.as_ref(), &session)?;
                                if !allowed.iter().any(|child| {
                                    child["location"].is_object()
                                        && child["location"]["env"] == args["location"]["env"]
                                }) {
                                    return Err(forbidden(
                                        "This session does not work on that environment",
                                    ));
                                }
                            }
                            Ok(service
                                .agent(session, method, args, &ctx.context)
                                .await?
                                .into())
                        })
                    })?;
                }
                Ok(members)
            },
            ServeOptions::default(),
        )
    }))
}

fn size(value: &Value) -> Result<(u16, u16)> {
    let get = |name: &str| {
        value[name]
            .as_u64()
            .filter(|n| (1..=1000).contains(n))
            .map(|n| n as u16)
    };
    get("cols")
        .zip(get("rows"))
        .ok_or_else(|| invalid("Invalid terminal size"))
}

fn list_event(service: &Service, session: &str, kind: &str) -> Value {
    let terminals: Vec<_> = service
        .list(session)
        .iter()
        .map(|terminal| lock(&terminal.info).clone())
        .collect();
    json!({"type":kind,"terminals":terminals})
}

async fn send(socket: &mut WebSocket, event: Value) -> bool {
    socket
        .send(Message::Text(event.to_string().into()))
        .await
        .is_ok()
}

async fn channel(
    mut socket: WebSocket,
    request: HttpRequest,
    channels: Arc<Channels>,
    service: Arc<Service>,
) -> HttpResult<()> {
    let token = form_urlencoded::parse(request.query.as_bytes())
        .find(|(key, _)| key == "token")
        .map(|(_, v)| v)
        .unwrap_or_default();
    let principal = request
        .principal
        .as_ref()
        .and_then(|value| Principal::deserialize(value).ok());
    let view = if owner(principal) {
        channels.take(&token)
    } else {
        None
    };
    let Some(view) = view else {
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code: 1008,
                reason: "Forbidden".into(),
            })))
            .await;
        return Ok(());
    };
    let mut events = view.events.subscribe();
    let mut closed = view.closed.subscribe();
    let mut attached = view.attached.subscribe();
    let mut service_updates = service.changes.subscribe();
    let mut ping = tokio::time::interval(Duration::from_secs(15));
    ping.tick().await;
    let mut current = None::<Arc<Terminal>>;
    let mut output = None;
    let mut offset = 0_u64;
    let mut sent = 0_u64;
    let mut snapshot_end = 0_u64;
    let mut epoch = u64::MAX;
    if !send(
        &mut socket,
        list_event(&service, &view.session, "terminals"),
    )
    .await
    {
        return Ok(());
    }
    loop {
        if *closed.borrow_and_update() {
            break;
        }
        let state = attached.borrow_and_update().clone();
        let attaching = state.epoch != epoch;
        if attaching {
            epoch = state.epoch;
            offset = 0;
            sent = 0;
            snapshot_end = 0;
            current = state.terminal.clone();
            output = current.as_ref().map(|terminal| terminal.subscribe());
        }
        if let Some(terminal) = &current {
            let credit = WINDOW.saturating_sub(sent.saturating_sub(state.ack.max(snapshot_end)));
            if credit > 0 {
                let (bytes, skipped) = terminal.bytes(offset, credit as usize);
                if attaching || skipped > 0 {
                    let event = json!({"type":"attached","terminal":*lock(&terminal.info)});
                    if !send(&mut socket, event).await {
                        break;
                    }
                    let (end, snapshot) = terminal.snapshot();
                    offset = end;
                    sent += snapshot.len() as u64;
                    snapshot_end = sent;
                    if socket.send(Message::Binary(snapshot.into())).await.is_err() {
                        break;
                    }
                    continue;
                }
                if !bytes.is_empty() {
                    offset += bytes.len() as u64;
                    sent += bytes.len() as u64;
                    if socket.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                    continue;
                }
            }
        }
        tokio::select! {
            _ = closed.changed() => break,
            _ = attached.changed() => {},
            _ = service_updates.changed() => {
                if !send(&mut socket,list_event(&service,&view.session,"terminals")).await { break; }
            }
            _ = async { match &mut output {
                Some(output) => { let _ = output.changed().await; }
                None => std::future::pending::<()>().await,
            } } => {},
            _ = ping.tick() => if !send(&mut socket,json!({"type":"ping"})).await { break; },
            event = events.recv() => if let Ok(event) = event && !send(&mut socket,event).await { break; },
            message = socket.recv() => match message {
                Some(Ok(Message::Binary(bytes))) => if let Some(terminal) = &current && terminal.send(bytes.to_vec()).await.is_err() { break; },
                Some(Ok(Message::Text(_))) => {
                    let _ = socket.send(Message::Close(Some(CloseFrame{code:1003,reason:"Terminal channel accepts bytes only".into()}))).await;
                    break;
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {},
                _ => break,
            }
        }
    }
    Ok(())
}
