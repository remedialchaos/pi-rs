use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::{Router, extract::RawQuery, http::StatusCode, response::Html, routing::get};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use indexmap::IndexMap;
use reqwest::{Client, Method, RequestBuilder};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::auth::{
    AuthEvent, AuthInteraction, AuthPrompt, LoginOptions, ModelAuth, OAuthAuth, OAuthCredential,
    PromptKind, SelectOption,
};
use crate::error::{Error, Result};
use crate::http::abortable;
use crate::types::unix_millis;

mod browser;
mod device;

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const ANTHROPIC: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CODEX: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CHATGPT_SCOPE: &str = "chatgpt.tokens.use.direct";
const CHATGPT_RESOURCE: &str = "https://api.openai.com/v1";
const COPILOT: &str = "Iv1.b507a08c87ecfe98";
const KIMI: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const META: &str = "1031625952748946";
const XAI: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const RADIUS: &str = "pi-gateway";
const RADIUS_SCOPE: &str = "gateway offline_access";
const DEFAULT_RADIUS: &str = "https://radius.pi.dev";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Flow {
    Anthropic,
    Codex,
    ChatGpt,
    Copilot,
    OpenRouter,
    Kimi,
    Meta,
    Xai,
    Radius,
}
impl Flow {
    pub fn name(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic (Claude Pro/Max)",
            Self::Codex => "OpenAI (ChatGPT Plus/Pro)",
            Self::ChatGpt => "OpenAI (ChatGPT subscription)",
            Self::Copilot => "GitHub Copilot",
            Self::OpenRouter => "OpenRouter OAuth",
            Self::Kimi => "Kimi Code (subscription)",
            Self::Meta => "Meta (Muse subscription)",
            Self::Xai => "xAI (Grok/X subscription)",
            Self::Radius => "Radius",
        }
    }
}
#[derive(Clone)]
pub struct BuiltinOAuth {
    flow: Flow,
    client: Client,
    hosts: Arc<IndexMap<String, Url>>,
    callback_host: String,
    callback_port: Option<u16>,
    radius_name: String,
    radius_gateway: Url,
}
impl BuiltinOAuth {
    pub fn new(flow: Flow) -> Result<Self> {
        Ok(Self {
            flow,
            client: Client::builder().build()?,
            hosts: Arc::default(),
            callback_host: std::env::var("PI_OAUTH_CALLBACK_HOST")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "127.0.0.1".into()),
            callback_port: None,
            radius_name: "Radius".into(),
            radius_gateway: Url::parse(DEFAULT_RADIUS)?,
        })
    }
    pub fn with_client(mut self, client: Client) -> Self {
        self.client = client;
        self
    }
    /// Redirect requests for a named authority to an explicitly configured endpoint. The OAuth redirect URI is unchanged.
    pub fn with_endpoint(mut self, authority: impl Into<String>, endpoint: Url) -> Self {
        Arc::make_mut(&mut self.hosts).insert(authority.into(), endpoint);
        self
    }
    pub fn with_callback_address(mut self, host: impl Into<String>, port: u16) -> Self {
        self.callback_host = host.into();
        self.callback_port = Some(port);
        self
    }
    pub fn with_radius_gateway(mut self, name: impl Into<String>, gateway: Url) -> Result<Self> {
        if trusted_url(gateway.as_str(), false).is_none() {
            return Err(oauth("Untrusted Radius gateway URL"));
        }
        self.radius_name = name.into();
        self.radius_gateway = gateway;
        Ok(self)
    }
    fn request(&self, method: Method, url: &str) -> Result<RequestBuilder> {
        let url = Url::parse(url)?;
        let redirected = if let Some(base) = url.host_str().and_then(|h| self.hosts.get(h)) {
            let mut updated = base.clone();
            updated.set_path(url.path());
            updated.set_query(url.query());
            updated
        } else {
            url
        };
        Ok(self
            .client
            .request(method, redirected)
            .header("accept", "application/json"))
    }
    async fn form(
        &self,
        url: &str,
        fields: &[(&str, &str)],
        signal: &CancellationToken,
    ) -> Result<Reply> {
        execute(self.request(Method::POST, url)?.form(fields), signal).await
    }
    async fn post(&self, url: &str, fields: &Value, signal: &CancellationToken) -> Result<Reply> {
        execute(self.request(Method::POST, url)?.json(fields), signal).await
    }
    fn radius_url(&self, path: &str) -> Result<Url> {
        Ok(self.radius_gateway.join(path)?)
    }
}
#[async_trait]
impl OAuthAuth for BuiltinOAuth {
    fn name(&self) -> &str {
        if self.flow == Flow::Radius {
            &self.radius_name
        } else {
            self.flow.name()
        }
    }
    fn is_subscription(&self) -> bool {
        !matches!(self.flow, Flow::OpenRouter | Flow::Radius)
    }
    fn login_label(&self) -> Option<&str> {
        match self.flow {
            Flow::ChatGpt => Some("Sign in with ChatGPT"),
            Flow::OpenRouter => Some("Sign in with OpenRouter"),
            Flow::Kimi => Some("Sign in with Kimi Code"),
            Flow::Meta => Some("Sign in with Meta"),
            Flow::Xai => Some("Sign in with SuperGrok or X Premium"),
            _ => None,
        }
    }
    async fn login(
        &self,
        ui: &dyn AuthInteraction,
        opts: &LoginOptions,
    ) -> Result<OAuthCredential> {
        let result = match self.flow {
            Flow::Anthropic => self.anthropic(ui, opts).await,
            Flow::ChatGpt => self.chatgpt(ui, opts).await,
            Flow::OpenRouter => self.openrouter(ui, opts).await,
            Flow::Codex => self.codex(ui, opts).await,
            Flow::Radius => self.radius(ui, opts).await,
            Flow::Copilot => self.copilot(ui, opts).await,
            Flow::Kimi | Flow::Meta | Flow::Xai => self.device_login(ui, opts, self.flow).await,
        };
        if opts.signal.is_cancelled() {
            Err(oauth("Login cancelled"))
        } else {
            result
        }
    }
    async fn refresh(
        &self,
        current: OAuthCredential,
        signal: CancellationToken,
    ) -> Result<OAuthCredential> {
        match self.flow {
            Flow::Anthropic => self.anthropic_refresh(&current.refresh, &signal).await,
            Flow::ChatGpt => self.chatgpt_refresh(&current, &signal).await,
            Flow::Codex => self.codex_refresh(&current.refresh, &signal).await,
            Flow::OpenRouter => Ok(current),
            Flow::Kimi | Flow::Meta | Flow::Xai => self.device_refresh(current, &signal).await,
            Flow::Copilot => self.copilot_refresh(&current, &signal).await,
            Flow::Radius => {
                self.radius_token(
                    &[
                        ("grant_type", "refresh_token"),
                        ("client_id", RADIUS),
                        ("refresh_token", &current.refresh),
                    ],
                    &signal,
                )
                .await
            }
        }
    }
    async fn to_auth(&self, c: &OAuthCredential) -> Result<ModelAuth> {
        if self.flow == Flow::Kimi {
            return Ok(ModelAuth {
                headers: Some(IndexMap::from([(
                    "Authorization".into(),
                    Some(format!("Bearer {}", c.access)),
                )])),
                ..ModelAuth::default()
            });
        }
        let base_url = (self.flow == Flow::Copilot)
            .then(|| device::copilot_base(&c.access, device::enterprise(c).as_deref()));
        Ok(ModelAuth {
            api_key: Some(c.access.clone()),
            base_url,
            headers: None,
        })
    }
}
fn oauth(message: impl Into<String>) -> Error {
    Error::OAuth(message.into())
}
fn string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value[key].as_str().filter(|s| !s.trim().is_empty())
}
fn field<'a>(value: &'a Value, key: &str, name: &str) -> Result<&'a str> {
    string(value, key).ok_or_else(|| oauth(format!("Invalid {name} OAuth response field: {key}")))
}
fn numeric(value: &Value, key: &str) -> Option<u64> {
    let n = value[key].as_f64()?;
    (n.is_finite() && n >= 0.0 && n < u64::MAX as f64).then_some(n as u64)
}
fn positive(value: &Value, key: &str) -> Option<u64> {
    numeric(value, key).filter(|v| *v > 0)
}
fn expiry(seconds: u64, skew: u64) -> u64 {
    unix_millis()
        .saturating_add(seconds.saturating_mul(1000))
        .saturating_sub(skew)
}
fn token(
    val: &Value,
    name: &str,
    skew: u64,
    previous: Option<&str>,
    ttl: Option<u64>,
) -> Result<OAuthCredential> {
    let expires = positive(val, "expires_in")
        .or_else(|| {
            if val.get("expires_in").is_none() {
                ttl
            } else {
                None
            }
        })
        .ok_or_else(|| oauth(format!("Invalid {name} OAuth response field: expires_in")))?;
    let refresh = string(val, "refresh_token")
        .or_else(|| {
            if val.get("refresh_token").is_none() {
                previous
            } else {
                None
            }
        })
        .ok_or_else(|| {
            oauth(format!(
                "Invalid {name} OAuth response field: refresh_token"
            ))
        })?;
    Ok(OAuthCredential {
        access: field(val, "access_token", name)?.into(),
        refresh: refresh.into(),
        expires: expiry(expires, skew),
        details: Map::new(),
    })
}
struct Reply {
    status: reqwest::StatusCode,
    body: Value,
    text: String,
}
impl Reply {
    fn ok(&self) -> bool {
        self.status.is_success()
    }
    fn error_code(&self) -> Option<&str> {
        string(&self.body, "error").or_else(|| string(&self.body["error"], "code"))
    }
    fn detail(&self) -> &str {
        ["error_description", "detail", "message", "error"]
            .into_iter()
            .find_map(|k| string(&self.body, k))
            .or_else(|| string(&self.body["error"], "message"))
            .unwrap_or_default()
    }
    fn required(self, label: &str) -> Result<Value> {
        if !self.ok() {
            let detail = self.detail();
            let body = if !detail.is_empty() {
                detail
            } else if !self.text.is_empty() {
                self.text.as_str()
            } else {
                self.status.canonical_reason().unwrap_or_default()
            };
            return Err(oauth(format!(
                "{label} ({}): {}",
                self.status.as_u16(),
                crate::http::truncate_error_text(body, 4000)
            )));
        }
        if self.body.is_null() {
            Err(oauth(format!("{label}: invalid JSON")))
        } else {
            Ok(self.body)
        }
    }
}
async fn execute(builder: RequestBuilder, signal: &CancellationToken) -> Result<Reply> {
    abortable(signal, async {
        let response = builder.timeout(Duration::from_secs(30)).send().await?;
        let status = response.status();
        let text = response.text().await?;
        let body = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ok(Reply { status, body, text })
    })
    .await
}
fn trusted_url(input: &str, https_only: bool) -> Option<Url> {
    let url = Url::parse(input).ok()?;
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    match url.scheme() {
        "https" if url.has_host() => Some(url),
        "http"
            if !https_only
                && url
                    .host_str()
                    .is_some_and(|h| matches!(h, "localhost" | "127.0.0.1" | "[::1]")) =>
        {
            Some(url)
        }
        _ => None,
    }
}
fn random_value() -> String {
    URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}
fn pkce() -> (String, String) {
    let verifier = random_value();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}
fn authorization(url: &str, fields: &[(&str, &str)]) -> Result<String> {
    let mut url = Url::parse(url)?;
    url.query_pairs_mut().clear().extend_pairs(fields);
    Ok(url.into())
}
fn notify(ui: &dyn AuthInteraction, url: String, info: &str) {
    ui.notify(AuthEvent::AuthUrl {
        url,
        instructions: Some(info.into()),
    });
}
async fn prompt(
    ui: &dyn AuthInteraction,
    signal: &CancellationToken,
    message: &str,
    kind: PromptKind,
    placeholder: Option<&str>,
) -> Result<String> {
    abortable(
        signal,
        ui.prompt(AuthPrompt {
            kind,
            message: message.into(),
            placeholder: placeholder.map(str::to_owned),
            signal: signal.clone(),
        }),
    )
    .await
}
async fn choose(
    ui: &dyn AuthInteraction,
    opts: &LoginOptions,
    message: &str,
    choices: &[(&str, &str)],
) -> Result<String> {
    let list = choices
        .iter()
        .map(|(id, label)| SelectOption {
            id: (*id).into(),
            label: (*label).into(),
            description: None,
        })
        .collect();
    prompt(ui, &opts.signal, message, PromptKind::Select(list), None).await
}
fn progress(ui: &dyn AuthInteraction, message: &str) {
    ui.notify(AuthEvent::Progress(message.into()));
}
#[derive(Default)]
struct Grant {
    code: String,
    state: Option<String>,
    client_id: Option<String>,
}
fn query(value: &str) -> IndexMap<String, String> {
    url::form_urlencoded::parse(value.as_bytes())
        .into_owned()
        .collect()
}
fn grant(
    mut args: IndexMap<String, String>,
    state: Option<&str>,
    require_state: bool,
    require_client: bool,
) -> Result<Grant> {
    if let Some(error) = args.shift_remove("error") {
        return Err(oauth(format!("Authorization failed: {error}")));
    }
    let code = args
        .shift_remove("code")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| oauth("Missing authorization code"))?;
    let actual = args.shift_remove("state");
    if let Some(expected) = state {
        if require_state && actual.is_none() {
            return Err(oauth("Missing OAuth state"));
        }
        if actual.as_deref().is_some_and(|s| s != expected) {
            return Err(oauth("OAuth state mismatch"));
        }
    }
    let client_id = args
        .shift_remove("client_id")
        .filter(|s| !s.trim().is_empty());
    if require_client && client_id.is_none() {
        return Err(oauth(
            "OpenAI OAuth registration callback did not contain an issued client ID",
        ));
    }
    Ok(Grant {
        code,
        state: actual,
        client_id,
    })
}
fn parse_code(
    input: &str,
    redirect: Option<&str>,
    state: Option<&str>,
    require_client: bool,
) -> Result<Grant> {
    let input = input.trim();
    if let Ok(url) = Url::parse(input) {
        if let Some(expected) = redirect {
            let parsed = Url::parse(expected)?;
            if url.origin() != parsed.origin() || url.path() != parsed.path() {
                return Err(oauth(format!(
                    "The pasted callback URL must start with {expected}"
                )));
            }
        }
        return grant(
            query(url.query().unwrap_or_default()),
            state,
            require_client,
            require_client,
        );
    }
    if redirect.is_some() {
        return Err(oauth("Paste the full callback URL from the browser"));
    }
    let fields = if let Some((code, state)) = input.split_once('#') {
        IndexMap::from([("code".into(), code.into()), ("state".into(), state.into())])
    } else if input.contains("code=") {
        query(input)
    } else {
        IndexMap::from([("code".into(), input.into())])
    };
    grant(fields, state, false, false)
}
struct Callback {
    url: String,
    receiver: oneshot::Receiver<Result<Grant>>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Callback {
    fn drop(&mut self) {
        self.server.abort();
    }
}
async fn callback(
    host: &str,
    port: u16,
    redirect_host: &str,
    path: &str,
    state: Option<&str>,
    client: bool,
) -> Result<Callback> {
    let listener = TcpListener::bind((host, port)).await?;
    let port = listener.local_addr()?.port();
    let url = format!("http://{redirect_host}:{port}{path}");
    let (send, receiver) = oneshot::channel();
    let send = Arc::new(Mutex::new(Some(send)));
    let state = state.map(str::to_owned);
    let app=Router::new().route(path,get(move |RawQuery(raw):RawQuery|{let send=send.clone();let state=state.clone();async move{
        let args=query(raw.as_deref().unwrap_or_default());
        let authorization_error=args.contains_key("error");
        let result=grant(args,state.as_deref(),state.is_some(),client);
        let success=result.is_ok();
        if (success||authorization_error)&&let Some(sender)=send.lock().unwrap_or_else(|e|e.into_inner()).take(){let _=sender.send(result);}
        if success{(StatusCode::OK,Html("<!doctype html><title>Authentication complete</title><h1>Authentication complete</h1><p>You can close this window.</p>"))}
        else{(StatusCode::BAD_REQUEST,Html("<!doctype html><title>Authentication failed</title><h1>Invalid authorization callback</h1>"))}
    }}));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(Callback {
        url,
        receiver,
        server,
    })
}
async fn await_code(
    ui: &dyn AuthInteraction,
    opts: &LoginOptions,
    callback: Option<&mut Callback>,
    message: &str,
    redirect: &str,
    state: Option<&str>,
    require_client: bool,
) -> Result<Grant> {
    let local = opts.signal.child_token();
    let guard = local.clone().drop_guard();
    let manual = async {
        let input = prompt(ui, &local, message, PromptKind::ManualCode, Some(redirect)).await?;
        parse_code(
            &input,
            require_client.then_some(redirect),
            state,
            require_client,
        )
    };
    let result = if let Some(c) = callback {
        tokio::select! {
            biased;
            _=opts.signal.cancelled()=>Err(Error::Aborted),
            value=&mut c.receiver=>value.map_err(|_|oauth("OAuth callback did not complete."))?,
            input=manual=>input,
        }
    } else {
        manual.await
    };
    drop(guard);
    result
}
async fn sleep(signal: &CancellationToken, duration: Duration) -> Result<()> {
    abortable(signal, async {
        tokio::time::sleep(duration).await;
        Ok(())
    })
    .await
}
enum Poll<T> {
    Ready(T),
    Pending,
    Slow(Option<u64>),
}
async fn poll<T, F, Fut>(
    interval: Option<u64>,
    lifetime: Option<u64>,
    first_wait: bool,
    signal: &CancellationToken,
    mut work: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Poll<T>>>,
{
    let start = Instant::now();
    let end = lifetime.and_then(|n| start.checked_add(Duration::from_secs(n)));
    let mut duration = Duration::from_secs(interval.unwrap_or(5).max(1));
    let mut slow = false;
    if first_wait {
        sleep(
            signal,
            end.map_or(duration, |d| {
                duration.min(d.saturating_duration_since(Instant::now()))
            }),
        )
        .await?;
    }
    while end.is_none_or(|d| Instant::now() < d) {
        match abortable(signal, work()).await? {
            Poll::Ready(value) => return Ok(value),
            Poll::Pending => {}
            Poll::Slow(next) => {
                slow = true;
                duration = next
                    .filter(|v| *v > 0)
                    .map(Duration::from_secs)
                    .unwrap_or_else(|| duration.saturating_add(Duration::from_secs(5)));
            }
        }
        let delay = end.map_or(duration, |d| {
            duration.min(d.saturating_duration_since(Instant::now()))
        });
        if delay.is_zero() {
            break;
        }
        sleep(signal, delay).await?;
    }
    Err(oauth(if slow {
        "Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again."
    } else {
        "Device flow timed out"
    }))
}
fn device_event(
    ui: &dyn AuthInteraction,
    code: &str,
    uri: &str,
    interval: Option<u64>,
    ttl: Option<u64>,
) {
    ui.notify(AuthEvent::DeviceCode {
        user_code: code.into(),
        verification_uri: uri.into(),
        interval_seconds: interval.unwrap_or(5),
        expires_in_seconds: ttl,
        open_browser_on_confirm: true,
        copy_code_on_confirm: true,
    });
}
