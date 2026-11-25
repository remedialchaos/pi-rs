//! A Radius relay connection and its multiplexed loopback channels.

use std::{collections::HashMap, fmt::Display, sync::Arc, time::Duration};

use axum::body::{Body, Bytes};
use futures_util::{SinkExt, StreamExt, stream::unfold};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Uri, header};
use hyper::{body::Body as _, client::conn::http1};
use hyper_util::rt::TokioIo;
use pi_rs_chord::{Error, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    net::TcpStream,
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch},
    task::{AbortHandle, JoinSet},
    time::{Instant, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{Message, client::IntoClientRequest, protocol::WebSocketConfig},
};
use url::Url;
use uuid::Uuid;

use super::{
    brain::{Details, RadiusBrain, Status, SyncAccess},
    login::invalid,
    relay::{
        RELAY_ORIGIN_HEADER, RELAY_SECRET_HEADER, RELAY_SHELL_HEADER, RELAY_USER_HEADER, RelayUser,
        encode_relay_user,
    },
};

const HANDSHAKE: Duration = Duration::from_secs(30);
const HOST_PROTOCOLS: &str =
    "pi-server.host.v5, pi-server.host.v4, pi-server.host.v3, pi-server.host.v2";
const CHANNEL_PROTOCOL: &str = "pi-server.channel.v1";
const MAX_PAYLOAD: usize = 32 * 1024 * 1024;
const BODY_CHUNK: usize = 256 * 1024;
const DEFAULT_WINDOW: usize = BODY_CHUNK;
const FRAME_HEADER: usize = 18;
const VERSION: u8 = 2;
const BINARY: u8 = 1;
const TEXT: u8 = 2;
const END: u8 = 3;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
const OUTPUT_BUDGET: usize = 1024 * 1024;
const INPUT_BUDGET: usize = 8 * 1024 * 1024;

#[derive(Clone)]
struct Output {
    sender: mpsc::Sender<(Message, OwnedSemaphorePermit)>,
    budget: Arc<Semaphore>,
}

impl Output {
    async fn send(&self, message: Message) -> Result<()> {
        let budget = self
            .budget
            .clone()
            .acquire_many_owned(weight(message.len(), OUTPUT_BUDGET))
            .await
            .map_err(|_| Error::Closed)?;
        self.sender
            .send((message, budget))
            .await
            .map_err(|_| Error::Closed)
    }
}

fn weight(size: usize, budget: usize) -> u32 {
    // Each budget is at most eight MiB; oversized frames consume all of it.
    size.clamp(1, budget) as u32
}

#[derive(Clone, Debug, Deserialize)]
struct Identity {
    kind: String,
    id: String,
    organization_id: String,
    email: Option<String>,
}

impl Identity {
    fn is_owner(&self, owner: &Self) -> bool {
        self.kind == owner.kind
            && self.id == owner.id
            && self.organization_id == owner.organization_id
    }
}

#[derive(Deserialize)]
struct Head {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Http,
    Ws,
    Stream,
}

#[derive(Deserialize)]
struct Open {
    connection_id: Uuid,
    kind: Kind,
    principal: Option<Identity>,
    public_origin: Option<String>,
    request: Option<Head>,
    window: Option<usize>,
    forward: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Control {
    Ping,
    Pong,
    ConnectionOpen(Box<Open>),
    ConnectionClose { connection_id: Uuid },
    Credit { connection_id: Uuid, bytes: usize },
}

struct Data {
    kind: u8,
    bytes: Bytes,
    _budget: Option<OwnedSemaphorePermit>,
}

struct Channel {
    incoming: mpsc::Sender<Data>,
    credit: watch::Sender<usize>,
    task: AbortHandle,
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Target {
    host: &'static str,
    port: u16,
    forward_origin: Option<Url>,
    headers: HeaderMap,
    path: String,
    method: Method,
}

#[derive(Clone, Copy)]
pub struct Forward {
    pub port: u16,
    pub public: bool,
}

/// Resolves the live forward table contributed by the ports plugin.
pub type ResolveForward = Arc<dyn Fn(&str) -> Option<Forward> + Send + Sync>;

struct Relay<'a> {
    brain: &'a Arc<RadiusBrain>,
    identity: SyncAccess,
    owner: Identity,
    output: Output,
    flow_control: bool,
    channel_sockets: bool,
}

impl Relay<'_> {
    async fn open(&self, open: Open, tasks: &mut JoinSet<Uuid>) -> Result<Option<Channel>> {
        let id = open.connection_id;
        let owner = open
            .principal
            .as_ref()
            .is_some_and(|identity| identity.is_owner(&self.owner));
        let (incoming, receiver) = mpsc::channel(16);
        let (credit, credits) = watch::channel(0usize);
        let output = self.output.clone();
        if owner
            && matches!(open.kind, Kind::Ws)
            && open.request.is_none()
            && open.forward.is_none()
            && self.channel_sockets
        {
            let brain = self.brain.clone();
            let identity = self.identity.clone();
            let origin = brain.public_origin();
            let secret = self.brain.relay.secret().to_owned();
            let port = self.brain.port;
            let task = tasks.spawn(async move {
                let result: Result<()> = async {
                    let access = current_access(&brain, &identity).await?;
                    relay_client(&access, id, port, origin, &secret).await
                }
                .await;
                if let Err(error) = result {
                    tracing_error("Radius client channel", error);
                    let _ = close_channel(&output, id, 1011).await;
                }
                id
            });
            return Ok(Some(Channel {
                incoming,
                credit,
                task,
            }));
        }

        let member = open.principal.as_ref().filter(|identity| {
            !owner
                && identity.kind == "user"
                && identity
                    .email
                    .as_ref()
                    .is_some_and(|email| !email.is_empty())
                && open.forward.is_none()
        });
        let anonymous = open.principal.is_none() && open.forward.is_some();
        if matches!(open.kind, Kind::Stream)
            || !(owner || member.is_some() || anonymous)
            || open.request.is_none()
            || open.public_origin.is_none()
        {
            close_channel(&output, id, 1008).await?;
            return Ok(None);
        }
        let Some(head) = open.request else {
            return Ok(None);
        };
        let Some(origin) = open.public_origin.as_deref().and_then(parse_origin) else {
            close_channel(&output, id, 1008).await?;
            return Ok(None);
        };
        if !valid_path(&head.path) {
            close_channel(&output, id, 1008).await?;
            return Ok(None);
        }
        let (mut headers, shell) = request_headers(head.headers)?;
        let (host, port, forward_origin) = if let Some(forward) = open.forward {
            let target = (self.brain.forward)(&forward)
                .filter(|target| target.port != self.brain.port && (!anonymous || target.public));
            let Some(target) = target else {
                if matches!(open.kind, Kind::Http) {
                    respond(
                        &output,
                        id,
                        404,
                        "This port forward does not exist (any more).\n",
                    )
                    .await?;
                } else {
                    close_channel(&output, id, 1008).await?;
                }
                return Ok(None);
            };
            let authority = origin[url::Position::BeforeHost..url::Position::AfterPort].to_owned();
            if headers
                .get(header::ORIGIN)
                .and_then(|value| value.to_str().ok())
                == open.public_origin.as_deref()
            {
                headers.insert(
                    header::ORIGIN,
                    header_value(&format!("http://localhost:{}", target.port))?,
                );
            }
            headers.insert("x-forwarded-host", header_value(&authority)?);
            headers.insert("x-forwarded-proto", header_value(origin.scheme())?);
            ("localhost", target.port, Some(origin))
        } else {
            let Some(shell) = shell.as_deref().and_then(parse_origin) else {
                close_channel(&output, id, 1008).await?;
                return Ok(None);
            };
            let origin = origin.origin().ascii_serialization();
            let shell = shell.origin().ascii_serialization();
            let page = format!(
                "{shell}/pi/{}/{}",
                segment(&self.owner.organization_id),
                segment(&self.identity.server_id)
            );
            if let Err(error) = self
                .brain
                .remember(&self.identity, origin.clone(), page)
                .await
            {
                tracing_error("Radius address", error);
            }
            mark(
                &mut headers,
                self.brain.relay.secret(),
                &origin,
                Some(&shell),
                member,
            )?;
            ("127.0.0.1", self.brain.port, None)
        };
        let target = Target {
            host,
            port,
            forward_origin,
            headers,
            path: head.path,
            method: head.method.parse().map_err(invalid)?,
        };
        let brain = self.brain.clone();
        let identity = self.identity.clone();
        let window = self
            .flow_control
            .then_some(open.window.unwrap_or(DEFAULT_WINDOW));
        let dedicated = self.channel_sockets;
        let task = tasks.spawn(async move {
            let result = match open.kind {
                Kind::Http => mux_http(target, id, &output, receiver, window, credits).await,
                Kind::Ws if dedicated => match current_access(&brain, &identity).await {
                    Ok(access) => relay_socket(&access, id, target).await,
                    Err(error) => Err(error),
                },
                Kind::Ws => mux_socket(target, id, &output, receiver).await,
                Kind::Stream => Ok(()),
            };
            if let Err(error) = result {
                tracing_error("Radius relay channel", error);
                let _ = close_channel(&output, id, 1011).await;
            }
            id
        });
        Ok(Some(Channel {
            incoming,
            credit,
            task,
        }))
    }
}

#[derive(Deserialize)]
struct Context {
    actor: WireActor,
    organization: WireOrganization,
}
#[derive(Deserialize)]
struct WireActor {
    principal_id: String,
    principal_kind: String,
}
#[derive(Deserialize)]
struct WireOrganization {
    id: String,
}

pub(super) async fn connect(
    brain: &Arc<RadiusBrain>,
    details: &watch::Sender<Details>,
    status: &watch::Sender<Status>,
    replace: &mut bool,
    delay: &mut Duration,
) -> Result<()> {
    let identity = brain.sync_access().await?;
    let gateway = Url::parse(&identity.gateway).map_err(invalid)?;
    let context = brain
        .http
        .get(gateway.join("/v1/context").map_err(invalid)?)
        .bearer_auth(&identity.token)
        .header(header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(invalid)?
        .error_for_status()
        .map_err(invalid)?
        .json::<Context>()
        .await
        .map_err(invalid)?;
    let owner = Identity {
        kind: context.actor.principal_kind,
        id: context.actor.principal_id,
        organization_id: context.organization.id,
        email: None,
    };
    let name = details.borrow().name.clone();
    let mut url = server_url(&identity, "connect")?;
    if *replace {
        url.query_pairs_mut().append_pair("replace", "1");
    }
    if let Some(name) = &name {
        url.query_pairs_mut().append_pair("name", name);
    }
    let (socket, protocol) = websocket(&url, bearer(&identity, HOST_PROTOCOLS)?).await?;
    let channel_sockets = protocol.as_deref() == Some("pi-server.host.v5");
    let flow_control = matches!(
        protocol.as_deref(),
        Some("pi-server.host.v3" | "pi-server.host.v4" | "pi-server.host.v5")
    );
    status.send_replace(Status::Connected {
        server_id: identity.server_id.clone(),
        organization_id: owner.organization_id.clone(),
        name: name.clone(),
    });
    *replace = true;
    *delay = Duration::from_secs(1);
    let (write, read) = socket.split();
    let (sender, outgoing) = mpsc::channel(16);
    let output = Output {
        sender,
        budget: Arc::new(Semaphore::new(OUTPUT_BUDGET)),
    };
    let relay = Relay {
        brain,
        identity,
        owner,
        output,
        flow_control,
        channel_sockets,
    };
    let mut tasks = JoinSet::new();
    let writer = async {
        let mut write = write;
        let mut outgoing = outgoing;
        while let Some((message, _permit)) = outgoing.recv().await {
            timeout(Duration::from_secs(10), write.send(message))
                .await
                .map_err(invalid)?
                .map_err(invalid)?;
        }
        Ok(())
    };
    tokio::select! {
        result = writer => result,
        result = read_relay(&relay, read, &mut tasks, replace) => result,
        () = report(brain, &relay.identity, details.subscribe(), name) => Ok(()),
    }
}

async fn read_relay(
    relay: &Relay<'_>,
    mut read: futures_util::stream::SplitStream<Socket>,
    tasks: &mut JoinSet<Uuid>,
    replace: &mut bool,
) -> Result<()> {
    let mut channels = HashMap::<Uuid, Channel>::new();
    let budget = Arc::new(Semaphore::new(INPUT_BUDGET));
    let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
    let mut last_received = Instant::now();
    let mut last_ping = last_received;
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if last_received.elapsed() >= Duration::from_secs(60) {
                    return Err(Error::Invalid("no message from Radius for 60 s".into()));
                }
                if last_received.max(last_ping).elapsed() >= Duration::from_secs(20) {
                    send_control(&relay.output, json!({"type":"ping"})).await?;
                    last_ping = Instant::now();
                }
            }
            Some(Ok((task, id))) = tasks.join_next_with_id(), if !tasks.is_empty() => {
                if channels.get(&id).is_some_and(|channel| channel.task.id() == task) {
                    channels.remove(&id);
                }
            }
            message = read.next() => {
                let Some(message) = message else { return Ok(()); };
                last_received = Instant::now();
                match message.map_err(invalid)? {
                    Message::Text(text) => {
                        let wire: Value = serde_json::from_str(text.as_str())?;
                        if wire["version"] != VERSION { return Err(Error::Invalid("Unsupported Radius relay version".into())); }
                        match serde_json::from_value::<Control>(wire)? {
                            Control::Ping => send_control(&relay.output,json!({"type":"pong"})).await?,
                            Control::Pong => {}
                            Control::ConnectionOpen(open) => {
                                let id = open.connection_id;
                                if channels.contains_key(&id) || channels.len() >= 256 {
                                    close_channel(&relay.output, id, 1008).await?;
                                } else if let Some(channel) = relay.open(*open,tasks).await? {
                                    channels.insert(id,channel);
                                }
                            }
                            Control::ConnectionClose { connection_id } => { channels.remove(&connection_id); }
                            Control::Credit { connection_id, bytes } => {
                                if let Some(channel) = channels.get(&connection_id) {
                                    channel.credit.send_modify(|available| *available = available.saturating_add(bytes));
                                }
                            }
                        }
                    }
                    Message::Binary(bytes) => {
                        if let Some((id, mut data)) = frame(bytes)
                            && let Some(channel) = channels.get(&id)
                        {
                            let permit = budget.clone().try_acquire_many_owned(weight(data.bytes.len(), INPUT_BUDGET));
                            let refused = match permit {
                                Ok(permit) => {
                                    data._budget = Some(permit);
                                    matches!(channel.incoming.try_send(data), Err(mpsc::error::TrySendError::Full(_)))
                                }
                                Err(_) => true,
                            };
                            if refused {
                                channels.remove(&id);
                                close_channel(&relay.output, id, 1011).await?;
                            }
                        }
                    }
                    Message::Ping(bytes) => relay.output.send(Message::Pong(bytes)).await?,
                    Message::Close(close) => {
                        if close.as_ref().is_some_and(|close| u16::from(close.code) == 1012 && close.reason.as_str() == "Server host replaced") {
                            *replace = false;
                            return Err(Error::Invalid("another p2 with this server ID took over; waiting for it to disconnect".into()));
                        }
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }
    }
}

async fn report(
    brain: &RadiusBrain,
    connection: &SyncAccess,
    mut details: watch::Receiver<Details>,
    mut name: Option<String>,
) {
    let mut shares = None;
    loop {
        let wanted = details.borrow_and_update().clone();
        for (resource, update) in [
            (
                "shares",
                wanted
                    .shares
                    .as_ref()
                    .filter(|current| shares.as_ref() != Some(*current))
                    .map(|emails| json!({"emails":emails})),
            ),
            (
                "name",
                wanted
                    .name
                    .as_ref()
                    .filter(|current| name.as_ref() != Some(*current))
                    .map(|name| json!({"name":name})),
            ),
        ] {
            if let Some(update) = update {
                let result: Result<()> = async {
                    let access = current_access(brain, connection).await?;
                    let url = server_url(&access, resource)?;
                    brain
                        .http
                        .put(url)
                        .bearer_auth(access.token)
                        .json(&update)
                        .send()
                        .await
                        .map_err(invalid)?
                        .error_for_status()
                        .map_err(invalid)?;
                    Ok(())
                }
                .await;
                match result {
                    Ok(()) if resource == "shares" => shares.clone_from(&wanted.shares),
                    Ok(()) => name.clone_from(&wanted.name),
                    Err(error) => tracing_error("Radius update", error),
                }
            }
        }
        if details.changed().await.is_err() {
            return;
        }
    }
}

async fn current_access(brain: &RadiusBrain, connection: &SyncAccess) -> Result<SyncAccess> {
    let access = brain.sync_access().await?;
    if access.gateway != connection.gateway || access.server_id != connection.server_id {
        return Err(Error::Invalid("Radius login changed".into()));
    }
    Ok(access)
}

fn server_url(access: &SyncAccess, path: &str) -> Result<Url> {
    Url::parse(&access.gateway)
        .map_err(invalid)?
        .join(&format!(
            "/v1/servers/{}/{path}",
            segment(&access.server_id)
        ))
        .map_err(invalid)
}

fn segment(value: &str) -> String {
    const COMPONENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    percent_encoding::utf8_percent_encode(value, COMPONENT).to_string()
}

fn valid_path(value: &str) -> bool {
    value.starts_with('/')
        && !value.starts_with("//")
        && value.parse::<http::uri::PathAndQuery>().is_ok()
        && !value.contains('#')
}

fn parse_origin(value: &str) -> Option<Url> {
    let url = Url::parse(value).ok()?;
    (matches!(url.scheme(), "http" | "https") && url.origin().ascii_serialization() == value)
        .then_some(url)
}

fn header_value(value: &str) -> Result<HeaderValue> {
    HeaderValue::from_str(value).map_err(invalid)
}

fn request_headers(pairs: Vec<(String, String)>) -> Result<(HeaderMap, Option<String>)> {
    let mut headers = HeaderMap::new();
    let mut shell = None;
    for (name, value) in pairs {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(invalid)?;
        match name.as_str() {
            "x-radius-shell-origin" => shell = Some(value),
            "host"
            | "connection"
            | "upgrade"
            | "content-length"
            | "transfer-encoding"
            | "sec-websocket-key"
            | "sec-websocket-version"
            | "sec-websocket-extensions"
            | RELAY_SECRET_HEADER
            | RELAY_ORIGIN_HEADER
            | RELAY_SHELL_HEADER
            | RELAY_USER_HEADER => {}
            _ => {
                headers.append(name, header_value(&value)?);
            }
        }
    }
    Ok((headers, shell))
}

fn mark(
    headers: &mut HeaderMap,
    secret: &str,
    origin: &str,
    shell: Option<&str>,
    member: Option<&Identity>,
) -> Result<()> {
    let public =
        parse_origin(origin).ok_or_else(|| Error::Invalid("Invalid Radius relay origin".into()))?;
    headers.remove(header::COOKIE);
    headers.remove("forwarded");
    headers.remove("x-forwarded-for");
    // Chord uses the forwarded marker to select the authenticated relay path.
    headers.insert(
        "x-forwarded-host",
        header_value(&public[url::Position::BeforeHost..url::Position::AfterPort])?,
    );
    headers.insert("x-forwarded-proto", header_value(public.scheme())?);
    headers.insert(RELAY_SECRET_HEADER, header_value(secret)?);
    headers.insert(RELAY_ORIGIN_HEADER, header_value(origin)?);
    if let Some(shell) = shell {
        headers.insert(RELAY_SHELL_HEADER, header_value(shell)?);
    }
    if let Some(identity) = member
        && let Some(email) = &identity.email
    {
        headers.insert(
            RELAY_USER_HEADER,
            header_value(&encode_relay_user(&RelayUser {
                id: identity.id.clone(),
                email: email.clone(),
            }))?,
        );
    }
    Ok(())
}

fn bearer(identity: &SyncAccess, protocol: &str) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        header_value(&format!("Bearer {}", identity.token))?,
    );
    headers.insert(header::SEC_WEBSOCKET_PROTOCOL, header_value(protocol)?);
    Ok(headers)
}

async fn websocket(url: &Url, headers: HeaderMap) -> Result<(Socket, Option<String>)> {
    let mut url = url.clone();
    let scheme = match url.scheme() {
        "https" | "wss" => "wss",
        _ => "ws",
    };
    url.set_scheme(scheme)
        .map_err(|()| Error::Invalid("Invalid Radius WebSocket URL".into()))?;
    let mut request = url.as_str().into_client_request().map_err(invalid)?;
    request.headers_mut().extend(headers);
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_PAYLOAD + FRAME_HEADER))
        .max_frame_size(Some(MAX_PAYLOAD + FRAME_HEADER))
        .max_write_buffer_size(MAX_PAYLOAD * 2);
    let (socket, response) = timeout(
        HANDSHAKE,
        connect_async_with_config(request, Some(config), false),
    )
    .await
    .map_err(invalid)?
    .map_err(invalid)?;
    let protocol = response
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    Ok((socket, protocol))
}

fn frame(bytes: Bytes) -> Option<(Uuid, Data)> {
    if bytes.len() < FRAME_HEADER || bytes[0] != VERSION {
        return None;
    }
    let id = Uuid::from_slice(&bytes[2..FRAME_HEADER]).ok()?;
    Some((
        id,
        Data {
            kind: bytes[1],
            bytes: bytes.slice(FRAME_HEADER..),
            _budget: None,
        },
    ))
}

async fn send_data(output: &Output, id: Uuid, kind: u8, bytes: &[u8]) -> Result<()> {
    let mut frame = Vec::with_capacity(FRAME_HEADER + bytes.len());
    frame.extend([VERSION, kind]);
    frame.extend(id.as_bytes());
    frame.extend(bytes);
    output.send(Message::Binary(frame.into())).await
}

async fn send_control(output: &Output, mut data: Value) -> Result<()> {
    data["version"] = json!(VERSION);
    output.send(Message::Text(data.to_string().into())).await
}

async fn close_channel(output: &Output, id: Uuid, code: u16) -> Result<()> {
    send_control(
        output,
        json!({"type":"connection_close","connection_id":id,"code":code}),
    )
    .await
}

async fn respond(output: &Output, id: Uuid, status: u16, text: &str) -> Result<()> {
    send_control(
        output,
        json!({"type":"response","connection_id":id,"status":status,
        "headers":[["content-type","text/plain; charset=utf-8"]]}),
    )
    .await?;
    send_data(output, id, BINARY, text.as_bytes()).await?;
    send_data(output, id, END, &[]).await
}

fn tracing_error(action: &str, error: impl Display) {
    eprintln!("{action}: {error}");
}

struct AbortOnDrop(AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn http_request(
    target: &Target,
    body: Body,
) -> Result<(http::Response<hyper::body::Incoming>, AbortOnDrop)> {
    let address = (target.host, target.port);
    let stream = timeout(HANDSHAKE, TcpStream::connect(address))
        .await
        .map_err(invalid)?
        .map_err(invalid)?;
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
        .await
        .map_err(invalid)?;
    let task = AbortOnDrop(tokio::spawn(connection).abort_handle());
    let uri = target.path.parse::<Uri>().map_err(invalid)?;
    let mut request = Request::builder()
        .method(target.method.clone())
        .uri(uri)
        .body(body)
        .map_err(invalid)?;
    *request.headers_mut() = target.headers.clone();
    request.headers_mut().insert(
        header::HOST,
        header_value(&format!("{}:{}", target.host, target.port))?,
    );
    Ok((sender.send_request(request).await.map_err(invalid)?, task))
}

fn response_headers(headers: &HeaderMap, target: &Target) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            let value = value.to_str().ok()?;
            let mut rewritten = None;
            if name == header::LOCATION
                && let Some(origin) = &target.forward_origin
            {
                for host in ["localhost", "127.0.0.1"] {
                    let local = format!("http://{host}:{}", target.port);
                    if let Some(after) = value.strip_prefix(&local)
                        && (after.is_empty() || after.starts_with(['/', '?']))
                    {
                        rewritten =
                            Some(format!("{}{after}", origin.origin().ascii_serialization()));
                        break;
                    }
                }
            }
            Some((
                name.to_string(),
                rewritten.unwrap_or_else(|| value.to_owned()),
            ))
        })
        .collect()
}

async fn mux_http(
    target: Target,
    id: Uuid,
    output: &Output,
    input: mpsc::Receiver<Data>,
    window: Option<usize>,
    mut credits: watch::Receiver<usize>,
) -> Result<()> {
    let flow = window.is_some();
    let request_body = Body::from_stream(unfold(
        (input, output.clone()),
        move |(mut input, output)| async move {
            let data = input.recv().await?;
            if data.kind == END {
                return None;
            }
            if flow && !data.bytes.is_empty() {
                let message = json!({"type":"credit","connection_id":id,"bytes":data.bytes.len()});
                let _ = send_control(&output, message).await;
            }
            Some((
                Ok::<_, std::convert::Infallible>(data.bytes),
                (input, output),
            ))
        },
    ));
    let (response, _connection) = match http_request(&target, request_body).await {
        Ok(response) => response,
        Err(error) if target.forward_origin.is_some() => {
            tracing_error("Radius forward", error);
            return respond(
                output,
                id,
                502,
                &format!(
                    "Nothing is listening on port {} of this device.\n",
                    target.port
                ),
            )
            .await;
        }
        Err(error) => return Err(error),
    };
    let (head, mut body) = response.into_parts();
    send_control(
        output,
        json!({"type":"response","connection_id":id,
        "status":head.status.as_u16(),"headers":response_headers(&head.headers,&target)}),
    )
    .await?;
    let mut sent = 0usize;
    while let Some(frame) =
        std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx)).await
    {
        let Ok(data) = frame.map_err(invalid)?.into_data() else {
            continue;
        };
        let mut remaining = data.as_ref();
        while !remaining.is_empty() {
            let available = if let Some(window) = window {
                loop {
                    let available = window
                        .saturating_add(*credits.borrow_and_update())
                        .saturating_sub(sent);
                    if available > 0 {
                        break available;
                    }
                    credits.changed().await.map_err(|_| Error::Closed)?;
                }
            } else {
                BODY_CHUNK
            };
            let size = remaining.len().min(available).min(BODY_CHUNK);
            send_data(output, id, BINARY, &remaining[..size]).await?;
            sent = sent.saturating_add(size);
            remaining = &remaining[size..];
        }
    }
    send_data(output, id, END, &[]).await
}

async fn loopback(target: Target) -> Result<Socket> {
    let url = Url::parse(&format!(
        "ws://{}:{}{}",
        target.host, target.port, target.path
    ))
    .map_err(invalid)?;
    Ok(websocket(&url, target.headers).await?.0)
}

async fn mux_socket(
    target: Target,
    id: Uuid,
    output: &Output,
    mut input: mpsc::Receiver<Data>,
) -> Result<()> {
    let (mut write, mut read) = loopback(target).await?.split();
    loop {
        tokio::select! {
            Some(data) = input.recv() => {
                let message = match data.kind {
                    TEXT => Message::Text(String::from_utf8_lossy(&data.bytes).into_owned().into()),
                    END => continue,
                    _ => Message::Binary(data.bytes),
                };
                timeout(HANDSHAKE,write.send(message)).await.map_err(invalid)?.map_err(invalid)?;
            }
            message = read.next() => match message {
                Some(Ok(Message::Binary(bytes))) => send_data(output,id,BINARY,&bytes).await?,
                Some(Ok(Message::Text(text))) => send_data(output,id,TEXT,text.as_bytes()).await?,
                Some(Ok(Message::Close(close))) => {
                    return close_channel(output,id,close.map_or(1000,|close| u16::from(close.code))).await;
                }
                Some(Err(error)) => return Err(invalid(error)),
                None => return close_channel(output,id,1000).await,
                _ => {}
            },
        }
    }
}

async fn channel_socket(access: &SyncAccess, id: Uuid) -> Result<Socket> {
    let url = server_url(access, &format!("channels/{id}"))?;
    Ok(websocket(&url, bearer(access, CHANNEL_PROTOCOL)?).await?.0)
}

async fn relay_socket(access: &SyncAccess, id: Uuid, target: Target) -> Result<()> {
    let (radius, local) = tokio::try_join!(channel_socket(access, id), loopback(target))?;
    pipe(radius, local).await
}

async fn pipe(left: Socket, right: Socket) -> Result<()> {
    let (left_tx, left_rx) = left.split();
    let (right_tx, right_rx) = right.split();
    tokio::select! {
        result = copy_socket(left_rx,right_tx) => result,
        result = copy_socket(right_rx,left_tx) => result,
    }
}

async fn copy_socket(
    mut source: futures_util::stream::SplitStream<Socket>,
    mut sink: futures_util::stream::SplitSink<Socket, Message>,
) -> Result<()> {
    while let Some(message) = source.next().await {
        let message = message.map_err(invalid)?;
        let close = matches!(message, Message::Close(_));
        if matches!(
            message,
            Message::Text(_) | Message::Binary(_) | Message::Close(_)
        ) {
            timeout(HANDSHAKE, sink.send(message))
                .await
                .map_err(invalid)?
                .map_err(invalid)?;
        }
        if close {
            return Ok(());
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct ClientRequest {
    #[serde(rename = "type")]
    kind: ClientKind,
    #[serde(default)]
    method: String,
    path: String,
    headers: HashMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum ClientKind {
    Http,
    Websocket,
}

async fn relay_client(
    access: &SyncAccess,
    id: Uuid,
    port: u16,
    origin: Option<String>,
    secret: &str,
) -> Result<()> {
    let mut radius = channel_socket(access, id).await?;
    let first = timeout(HANDSHAKE, radius.next()).await.map_err(invalid)?;
    let request = match first {
        Some(Ok(Message::Text(text))) => serde_json::from_str::<ClientRequest>(text.as_str()).ok(),
        _ => None,
    };
    let Some(request) = request.filter(|request| {
        valid_path(&request.path)
            && (matches!(request.kind, ClientKind::Websocket)
                || (1..=16).contains(&request.method.len())
                    && request.method.bytes().all(|b| b.is_ascii_uppercase()))
    }) else {
        radius
            .send(Message::Close(Some(
                tokio_tungstenite::tungstenite::protocol::CloseFrame {
                    code: 1008u16.into(),
                    reason: "".into(),
                },
            )))
            .await
            .map_err(invalid)?;
        return Ok(());
    };
    let origin = origin
        .filter(|origin| parse_origin(origin).is_some())
        .unwrap_or_else(|| "https://radius.invalid".into());
    let (mut headers, _) = request_headers(request.headers.into_iter().collect())?;
    mark(&mut headers, secret, &origin, None, None)?;
    headers.insert(header::ORIGIN, header_value(&origin)?);
    let method = if matches!(request.kind, ClientKind::Websocket) {
        Method::GET
    } else {
        request.method.parse().map_err(invalid)?
    };
    let target = Target {
        host: "127.0.0.1",
        port,
        headers,
        path: request.path,
        forward_origin: None,
        method,
    };
    match request.kind {
        ClientKind::Websocket => {
            let local = loopback(target).await?;
            radius
                .send(Message::Text(
                    json!({"status":101,"headers":[],"origin":origin})
                        .to_string()
                        .into(),
                ))
                .await
                .map_err(invalid)?;
            pipe(radius, local).await
        }
        ClientKind::Http => client_http(radius, target, &origin).await,
    }
}

async fn client_http(radius: Socket, target: Target, origin: &str) -> Result<()> {
    let (mut write, read) = radius.split();
    let body = Body::from_stream(unfold(read, |mut read| async move {
        loop {
            match read.next().await? {
                Ok(Message::Binary(bytes)) => return Some((Ok(bytes), read)),
                Ok(Message::Text(_) | Message::Close(_)) => return None,
                Ok(_) => {}
                Err(error) => return Some((Err(error), read)),
            }
        }
    }));
    let (response, _connection) = http_request(&target, body).await?;
    let (head, mut body) = response.into_parts();
    write.send(Message::Text(json!({"status":head.status.as_u16(),"headers":response_headers(&head.headers,&target),"origin":origin}).to_string().into())).await.map_err(invalid)?;
    while let Some(frame) =
        std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx)).await
    {
        if let Ok(data) = frame.map_err(invalid)?.into_data() {
            for chunk in data.chunks(BODY_CHUNK) {
                timeout(
                    HANDSHAKE,
                    write.send(Message::Binary(Bytes::copy_from_slice(chunk))),
                )
                .await
                .map_err(invalid)?
                .map_err(invalid)?;
            }
        }
    }
    write.send(Message::Close(None)).await.map_err(invalid)
}
