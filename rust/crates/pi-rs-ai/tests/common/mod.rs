use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, StatusCode, Version},
    response::Response,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;

use pi_rs_ai::{Api, Context, Model, ProviderRequestOptions, StreamOptions};
pub type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
pub struct Capture {
    pub path: String,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub version: Version,
}
impl Capture {
    pub fn json(&self) -> TestResult<Value> {
        Ok(serde_json::from_slice(&self.body)?)
    }
}
struct ServerState {
    responses: Mutex<VecDeque<Response>>,
    requests: Mutex<Vec<Capture>>,
}
pub struct Server {
    inner: Arc<ServerState>,
    listener: tokio::task::JoinHandle<()>,
    pub url: String,
}
impl Server {
    pub async fn start(responses: Vec<Response>) -> TestResult<Self> {
        let socket = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", socket.local_addr()?);
        let inner = Arc::new(ServerState {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(vec![]),
        });
        let router = Router::new().fallback(handler).with_state(inner.clone());
        let listener = tokio::spawn(async move {
            let _ = axum::serve(socket, router).await;
        });
        Ok(Self {
            inner,
            listener,
            url,
        })
    }
    pub fn drain(&self) -> TestResult<Vec<Capture>> {
        Ok(std::mem::take(
            &mut *self
                .inner
                .requests
                .lock()
                .map_err(|_| "request capture poisoned")?,
        ))
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.listener.abort();
    }
}
async fn handler(State(state): State<Arc<ServerState>>, request: Request) -> Response {
    let (head, body) = request.into_parts();
    let Ok(body) = to_bytes(body, 4 * 1024 * 1024).await else {
        return plain(StatusCode::PAYLOAD_TOO_LARGE, "too large");
    };
    match state.requests.lock() {
        Ok(mut requests) => requests.push(Capture {
            path: head.uri.to_string(),
            headers: head.headers,
            body,
            version: head.version,
        }),
        Err(_) => return plain(StatusCode::INTERNAL_SERVER_ERROR, "capture poisoned"),
    }
    state
        .responses
        .lock()
        .ok()
        .and_then(|mut responses| responses.pop_front())
        .unwrap_or_else(|| plain(StatusCode::NOT_FOUND, "unexpected request"))
}
pub fn plain(status: StatusCode, body: impl Into<Body>) -> Response {
    let mut response = Response::new(body.into());
    *response.status_mut() = status;
    response
}
pub fn json_reply(value: Value) -> Response {
    let mut response = plain(StatusCode::OK, value.to_string());
    response.headers_mut().insert(
        "content-type",
        axum::http::HeaderValue::from_static("application/json"),
    );
    response
}
fn chunks(body: Vec<u8>, size: usize) -> Body {
    let bytes = Bytes::from(body);
    let pieces = (0..bytes.len()).step_by(size).map(move |start| {
        Ok::<_, std::io::Error>(bytes.slice(start..(start + size).min(bytes.len())))
    });
    Body::from_stream(futures_util::stream::iter(pieces))
}
pub fn sse(events: &[Value], mode: &str) -> Response {
    let bytes = events
        .iter()
        .map(|value| {
            format!(
                "event: {}{mode}data: {value}{mode}{mode}",
                value["type"].as_str().unwrap_or("message")
            )
        })
        .collect::<String>()
        .into_bytes();
    let mut response = plain(StatusCode::OK, chunks(bytes, 7));
    response.headers_mut().insert(
        "content-type",
        axum::http::HeaderValue::from_static("text/event-stream"),
    );
    response
}
pub fn eventstream(bytes: Vec<u8>) -> Response {
    let mut response = plain(StatusCode::OK, chunks(bytes, 29));
    response.headers_mut().insert(
        "content-type",
        axum::http::HeaderValue::from_static("application/vnd.amazon.eventstream"),
    );
    response
}
pub fn model(api: Api, provider: &str, id: &str, url: &str) -> TestResult<Model> {
    Ok(serde_json::from_value(json!({
        "api":api,"provider":provider,"id":id,"name":id,"baseUrl":url,"input":["text","image"],"contextWindow":100000,"maxTokens":8192,
        "reasoning":true,"cost":{"input":1,"output":2,"cacheRead":0.1,"cacheWrite":1.25}
    }))?)
}
pub fn context() -> TestResult<Context> {
    Ok(serde_json::from_value(
        json!({"systemPrompt":"be concise","messages":[{"role":"user","content":"hello","timestamp":0}]}),
    )?)
}
pub fn request<M>() -> TestResult<ProviderRequestOptions<M>> {
    Ok(ProviderRequestOptions {
        api_key: Some("fixture-key".into()),
        client: Some(reqwest::Client::builder().no_proxy().build()?),
        max_retries: Some(0),
        ..Default::default()
    })
}
pub fn stream_options() -> TestResult<StreamOptions> {
    Ok(StreamOptions {
        request: request()?,
        max_tokens: Some(1024),
        ..Default::default()
    })
}
