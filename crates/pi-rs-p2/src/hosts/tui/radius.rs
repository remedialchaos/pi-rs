//! Radius relay transport for `--radius [server-id]`
//! (ported from `apps/p2/src/hosts/tui-radius.ts`).

use crate::plugins::radius::login::gateway_from_env;
use futures_util::{SinkExt, StreamExt};
use http::{Request, Response, StatusCode, header};
use hyper::{body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use pi_rs_ai::{
    Error as AiError,
    auth::{AuthEvent, AuthInteraction, AuthPrompt, Credential, LoginOptions, OAuthAuth},
    providers::radius_provider,
};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    error::Error,
    fs,
    io::{self, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{Message, client::IntoClientRequest, protocol::Role, protocol::WebSocketConfig},
};

const CLIENT_SUBPROTOCOL: &str = "pi-server.client.v2";
const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const BODY_CHUNK_BYTES: usize = 256 * 1024;
const REFRESH_MARGIN_MS: u64 = 5 * 60_000;
pub const RADIUS_POLL_MS: Duration = Duration::from_secs(10);

type UpstreamWs = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone, Serialize, Deserialize)]
struct RadiusLogin {
    gateway: String,
    credential: Credential,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TunnelHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub origin: String,
}

pub fn parse_tunnel_head(bytes: &[u8]) -> Result<TunnelHead, String> {
    let head: TunnelHead =
        serde_json::from_slice(bytes).map_err(|_| "Invalid answer through Radius".to_string())?;
    Ok(head)
}

pub fn is_radius_server_id(s: &str) -> bool {
    uuid::Uuid::parse_str(s).is_ok()
}

pub fn radius_login_path(config_dir: &Path) -> PathBuf {
    config_dir.join("radius.json")
}

fn read_login(path: &Path) -> Result<Option<RadiusLogin>, Box<dyn Error>> {
    let text = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let parsed: RadiusLogin = serde_json::from_str(&text).map_err(|_| {
        format!(
            "Invalid {}; delete it to sign in to Radius again",
            path.display()
        )
    })?;
    if !matches!(parsed.credential, Credential::OAuth(_)) {
        return Err(format!(
            "Invalid {}; delete it to sign in to Radius again",
            path.display()
        )
        .into());
    }
    Ok(Some(parsed))
}

fn save_login(config_dir: &Path, path: &Path, login: &RadiusLogin) -> Result<(), Box<dyn Error>> {
    let mut dirs = fs::DirBuilder::new();
    dirs.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        dirs.mode(0o700);
    }
    dirs.create(config_dir)?;
    let tmp = config_dir.join(format!(".radius-{}.tmp", uuid::Uuid::new_v4()));
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let write_res = (|| -> Result<(), Box<dyn Error>> {
        let mut f = opts.open(&tmp)?;
        serde_json::to_writer_pretty(&mut f, login)?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if write_res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_res
}

fn radius_oauth(gateway: &Url) -> Result<Arc<dyn OAuthAuth>, Box<dyn Error>> {
    Ok(radius_provider("radius", "Radius", gateway.clone())?
        .auth
        .oauth
        .ok_or("The Radius provider has no OAuth support")?)
}

struct DeviceCodePrompt;

impl AuthInteraction for DeviceCodePrompt {
    fn prompt<'a, 'async_trait>(
        &'a self,
        prompt: AuthPrompt,
    ) -> Pin<Box<dyn std::future::Future<Output = pi_rs_ai::Result<String>> + Send + 'async_trait>>
    where
        'a: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            if prompt.signal.is_cancelled() {
                Err(AiError::Aborted)
            } else {
                Ok("device-code".into())
            }
        })
    }

    fn notify(&self, event: AuthEvent) {
        if let AuthEvent::DeviceCode {
            verification_uri,
            user_code,
            ..
        } = event
        {
            eprintln!("Open {verification_uri} and enter the code {user_code}");
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Clone)]
pub struct RadiusClient {
    config_dir: PathBuf,
    path: PathBuf,
    state: Arc<Mutex<RadiusLogin>>,
}

impl RadiusClient {
    pub async fn sign_in(config_dir: &Path) -> Result<Self, Box<dyn Error>> {
        let path = radius_login_path(config_dir);
        let login = match read_login(&path)? {
            Some(existing) => existing,
            None => {
                let gateway = gateway_from_env()?;
                eprintln!("Signing in to Radius at {gateway}");
                let options = LoginOptions::default();
                let cred = radius_oauth(&gateway)?
                    .login(&DeviceCodePrompt, &options)
                    .await?;
                let created = RadiusLogin {
                    gateway: gateway.as_str().trim_end_matches('/').to_string(),
                    credential: Credential::OAuth(cred),
                };
                save_login(config_dir, &path, &created)?;
                created
            }
        };
        Ok(Self {
            config_dir: config_dir.to_owned(),
            path,
            state: Arc::new(Mutex::new(login)),
        })
    }

    pub async fn gateway(&self) -> String {
        self.state.lock().await.gateway.clone()
    }

    pub async fn access_token(&self) -> Result<String, Box<dyn Error>> {
        let mut guard = self.state.lock().await;
        let Credential::OAuth(ref cred) = guard.credential else {
            return Err("Invalid Radius credential".into());
        };
        if cred.expires.saturating_sub(now_ms()) > REFRESH_MARGIN_MS {
            return Ok(cred.access.clone());
        }
        let gateway_url = Url::parse(&guard.gateway)?;
        let oauth = radius_oauth(&gateway_url)?;
        let cred_clone = cred.clone();
        let refreshed: OAuthCredential = oauth
            .refresh(cred_clone, LoginOptions::default().signal)
            .await?;
        guard.credential = Credential::OAuth(refreshed.clone());
        save_login(&self.config_dir, &self.path, &guard)?;
        Ok(refreshed.access)
    }

    pub async fn resolve_server(&self, explicit: Option<&str>) -> Result<String, Box<dyn Error>> {
        if let Some(id) = explicit {
            return Ok(id.to_string());
        }
        let token = self.access_token().await?;
        let gateway = Url::parse(&self.gateway().await)?;
        let http = Client::builder().timeout(Duration::from_secs(30)).build()?;
        let response = http
            .get(gateway.join("/v1/servers")?)
            .bearer_auth(token)
            .header(header::ACCEPT, "application/json")
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(format!("Radius did not list your servers ({})", response.status()).into());
        }
        let value: Value = response.json().await?;
        let online: Vec<String> = value["servers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| s["status"].as_str() == Some("online"))
            .filter_map(|s| s["server_id"].as_str().map(str::to_owned))
            .collect();
        match online.len() {
            1 => Ok(online.into_iter().next().unwrap_or_default()),
            0 => Err("None of your p2 servers is connected to Radius".into()),
            _ => Err(format!(
                "Several of your servers are online; choose one with --radius <server-id>: {}",
                online.join(", ")
            )
            .into()),
        }
    }

    async fn open_tunnel_ws(&self, server_id: &str) -> Result<UpstreamWs, Box<dyn Error>> {
        let token = self.access_token().await?;
        let mut url = Url::parse(&self.gateway().await)?;
        url.set_scheme(if url.scheme() == "https" { "wss" } else { "ws" })
            .map_err(|_| "Invalid Radius gateway scheme")?;
        let mut url = url.join(&format!("/v1/servers/{server_id}/connect"))?;
        let _ = &mut url;
        let mut req = url.as_str().into_client_request()?;
        req.headers_mut()
            .insert(header::AUTHORIZATION, format!("Bearer {token}").parse()?);
        req.headers_mut()
            .insert("Sec-WebSocket-Protocol", CLIENT_SUBPROTOCOL.parse()?);
        let mut cfg = WebSocketConfig::default();
        cfg.max_message_size = Some(MAX_MESSAGE_BYTES);
        cfg.max_frame_size = Some(MAX_MESSAGE_BYTES);
        let (ws, _) = tokio::time::timeout(
            Duration::from_secs(10),
            connect_async_with_config(req, Some(cfg), false),
        )
        .await??;
        Ok(ws)
    }

    pub async fn tunnel_http(
        &self,
        server_id: &str,
        method: &str,
        path: &str,
        headers: Vec<(String, String)>,
        body: &[u8],
    ) -> Result<(TunnelHead, Vec<u8>), Box<dyn Error>> {
        let mut ws = self.open_tunnel_ws(server_id).await?;
        let header_map: serde_json::Map<String, Value> = headers
            .into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect();
        ws.send(Message::Text(
            json!({
                "type": "http",
                "method": method,
                "path": path,
                "headers": Value::Object(header_map),
            })
            .to_string()
            .into(),
        ))
        .await?;
        for chunk in body.chunks(BODY_CHUNK_BYTES) {
            ws.send(Message::Binary(chunk.to_vec().into())).await?;
        }
        ws.send(Message::Text(json!({ "type": "end" }).to_string().into()))
            .await?;

        let mut head: Option<TunnelHead> = None;
        let mut out = Vec::new();
        while let Some(msg) = ws.next().await {
            match msg? {
                Message::Text(txt) if head.is_none() => {
                    head = Some(parse_tunnel_head(txt.as_bytes())?);
                }
                Message::Binary(bin) if head.is_some() => {
                    out.extend_from_slice(&bin);
                }
                Message::Close(_) => break,
                Message::Ping(p) => {
                    let _ = ws.send(Message::Pong(p)).await;
                }
                _ => {}
            }
        }
        let head = head.ok_or("The server closed the connection without an answer")?;
        Ok((head, out))
    }
}

use pi_rs_ai::auth::OAuthCredential;

/// Holds the active loopback proxy exposing a Radius-connected server to standard HTTP/WS clients.
pub struct RadiusBridge {
    pub local_origin: Url,
    pub logical_origin: String,
    task: JoinHandle<()>,
}

impl Drop for RadiusBridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn connect_radius(
    config_dir: &Path,
    server: Option<&str>,
) -> Result<RadiusBridge, Box<dyn Error>> {
    let client = RadiusClient::sign_in(config_dir).await?;
    let server_id = client.resolve_server(server).await?;
    let (head, body) = client
        .tunnel_http(&server_id, "GET", "/owner/check", Vec::new(), &[])
        .await?;
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    if !(200..300).contains(&head.status) || parsed["owner"] != Value::Bool(true) {
        return Err(format!(
            "This server does not admit you as its owner through Radius ({})",
            head.status
        )
        .into());
    }
    let logical_origin = Url::parse(&head.origin)?.origin().ascii_serialization();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr: SocketAddr = listener.local_addr()?;
    let local_origin = Url::parse(&format!("http://{addr}"))?;

    let client_ref = client.clone();
    let server_ref = server_id.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let client = client_ref.clone();
            let server_id = server_ref.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(move |req: Request<Incoming>| {
                    let client = client.clone();
                    let server_id = server_id.clone();
                    async move { handle_bridge_request(req, client, server_id).await }
                });
                let _ = http1::Builder::new()
                    .serve_connection(io, service)
                    .with_upgrades()
                    .await;
            });
        }
    });

    Ok(RadiusBridge {
        local_origin,
        logical_origin,
        task,
    })
}

async fn handle_bridge_request(
    mut req: Request<Incoming>,
    client: RadiusClient,
    server_id: String,
) -> Result<Response<axum::body::Body>, hyper::Error> {
    let path = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());

    if let Some(key) = req
        .headers()
        .get(header::SEC_WEBSOCKET_KEY)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
    {
        let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
        let Ok(mut upstream) = client.open_tunnel_ws(&server_id).await else {
            return Ok(Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(axum::body::Body::empty())
                .unwrap_or_default());
        };
        if upstream
            .send(Message::Text(
                json!({ "type": "websocket", "path": path, "headers": {} })
                    .to_string()
                    .into(),
            ))
            .await
            .is_err()
        {
            return Ok(Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(axum::body::Body::empty())
                .unwrap_or_default());
        }
        let status_101 = matches!(
            upstream.next().await,
            Some(Ok(Message::Text(t))) if parse_tunnel_head(t.as_bytes()).is_ok_and(|h| h.status == 101)
        );
        if !status_101 {
            return Ok(Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(axum::body::Body::empty())
                .unwrap_or_default());
        }

        tokio::spawn(async move {
            let Ok(upgraded) = hyper::upgrade::on(&mut req).await else {
                return;
            };
            let local_ws =
                WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None).await;
            let (mut l_tx, mut l_rx) = local_ws.split();
            let (mut u_tx, mut u_rx) = upstream.split();
            let c2s = async {
                while let Some(Ok(msg)) = l_rx.next().await {
                    if msg.is_close() {
                        let _ = u_tx.close().await;
                        break;
                    }
                    if u_tx.send(msg).await.is_err() {
                        break;
                    }
                }
            };
            let s2c = async {
                while let Some(Ok(msg)) = u_rx.next().await {
                    if msg.is_close() {
                        let _ = l_tx.close().await;
                        break;
                    }
                    if l_tx.send(msg).await.is_err() {
                        break;
                    }
                }
            };
            tokio::select! {
                _ = c2s => {}
                _ = s2c => {}
            }
        });

        let resp = Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .header(header::CONNECTION, "Upgrade")
            .header(header::UPGRADE, "websocket")
            .header(header::SEC_WEBSOCKET_ACCEPT, accept)
            .body(axum::body::Body::empty())
            .unwrap_or_default();
        return Ok(resp);
    }

    use hyper::body::Body as _;
    let method = req.method().as_str().to_string();
    let req_headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
        .collect();
    let mut body_bytes = Vec::new();
    let mut incoming = req.into_body();
    while let Some(Ok(frame)) =
        std::future::poll_fn(|cx| Pin::new(&mut incoming).poll_frame(cx)).await
    {
        if let Ok(data) = frame.into_data() {
            body_bytes.extend_from_slice(&data);
        }
    }
    match client
        .tunnel_http(&server_id, &method, &path, req_headers, &body_bytes)
        .await
    {
        Ok((head, resp_body)) => {
            let mut builder = Response::builder().status(head.status);
            for (k, v) in head.headers {
                if !k.eq_ignore_ascii_case("transfer-encoding") {
                    builder = builder.header(k, v);
                }
            }
            Ok(builder
                .body(axum::body::Body::from(resp_body))
                .unwrap_or_default())
        }
        Err(_) => Ok(Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(axum::body::Body::empty())
            .unwrap_or_default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_server_uuid_and_tunnel_head_wire_format() -> Result<(), Box<dyn Error>> {
        assert!(is_radius_server_id("12345678-1234-1234-1234-1234567890ab"));
        assert!(!is_radius_server_id("https://example.com"));
        let head = parse_tunnel_head(
            br#"{"status":200,"headers":[["content-type","application/json"]],"origin":"https://host.example"}"#,
        )?;
        assert_eq!(head.status, 200);
        assert_eq!(head.origin, "https://host.example");
        assert!(parse_tunnel_head(br#"{"status":"bad"}"#).is_err());
        Ok(())
    }

    #[test]
    fn saves_and_loads_radius_login_file_with_oauth_validation() -> Result<(), Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        let path = radius_login_path(dir.path());
        assert!(read_login(&path)?.is_none());
        let login = RadiusLogin {
            gateway: "https://radius.pi.dev".into(),
            credential: Credential::OAuth(OAuthCredential {
                refresh: "r1".into(),
                access: "a1".into(),
                expires: 9_999_999_999_999,
                details: Default::default(),
            }),
        };
        save_login(dir.path(), &path, &login)?;
        let loaded = read_login(&path)?.ok_or("missing saved login")?;
        assert_eq!(loaded.gateway, "https://radius.pi.dev");
        Ok(())
    }
}
