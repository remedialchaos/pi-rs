use crate::{Error, Result};
use axum::{
    Router,
    extract::{RawQuery, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse},
    routing::get,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use std::{net::Ipv4Addr, sync::Arc, time::Duration};
use tokio::{
    net::TcpListener,
    sync::{Mutex, oneshot},
    task::JoinHandle,
};
use url::Url;

pub const CLIENT_ID: &str = "pi-gateway";
pub const SCOPE: &str = "gateway offline_access";
const PATH: &str = "/oauth/callback";

pub struct Pkce {
    pub verifier: String,
    challenge: String,
    state: String,
}
impl Pkce {
    pub fn generate() -> Result<Self> {
        let mut entropy = [0u8; 72];
        getrandom::fill(&mut entropy)
            .map_err(|e| Error::Protocol(format!("cannot generate OAuth PKCE material: {e}")))?;
        let verifier = URL_SAFE_NO_PAD.encode(&entropy[..48]);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Ok(Self {
            verifier,
            challenge,
            state: URL_SAFE_NO_PAD.encode(&entropy[48..]),
        })
    }
    pub fn authorize(&self, mut endpoint: Url, redirect: &Url) -> Url {
        endpoint.set_query(None);
        endpoint.query_pairs_mut().extend_pairs([
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", redirect.as_str()),
            ("scope", SCOPE),
            ("code_challenge", &self.challenge),
            ("code_challenge_method", "S256"),
            ("handoff", "url"),
            ("state", &self.state),
        ]);
        endpoint
    }
}

struct Callback {
    expected: String,
    result: Mutex<Option<oneshot::Sender<Result<String>>>>,
}

pub struct Loopback {
    pub redirect: Url,
    result: oneshot::Receiver<Result<String>>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<std::io::Result<()>>>,
}
impl Loopback {
    pub async fn start(pkce: &Pkce) -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let redirect = Url::parse(&format!("http://{}{PATH}", listener.local_addr()?))?;
        let (answer, result) = oneshot::channel();
        let (stop, shutdown) = oneshot::channel();
        let router = Router::new()
            .route(PATH, get(receive))
            .with_state(Arc::new(Callback {
                expected: pkce.state.clone(),
                result: Mutex::new(Some(answer)),
            }));
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = shutdown.await;
                })
                .await
        });
        Ok(Self {
            redirect,
            result,
            stop: Some(stop),
            task: Some(task),
        })
    }
    pub async fn callback(mut self, timeout: Duration) -> Result<String> {
        let result = tokio::time::timeout(timeout, &mut self.result)
            .await
            .map_err(|_| Error::Radius(0, "the sign-in timed out".into()))?
            .map_err(|_| Error::Radius(0, "the sign-in callback closed".into()))?;
        self.stop.take();
        // Let the browser's confirmation page finish writing after delivery through the oneshot.
        if let Some(task) = self.task.take() {
            drop(task);
        }
        result
    }
}
impl Drop for Loopback {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn receive(
    State(callback): State<Arc<Callback>>,
    RawQuery(query): RawQuery,
) -> impl IntoResponse {
    let mut state = None;
    let mut code = None;
    let mut error = None;
    let mut description = None;
    if let Some(query) = query.as_deref() {
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            match key.as_ref() {
                "state" if state.is_none() => state = Some(value),
                "code" if code.is_none() => code = Some(value),
                "error" if error.is_none() => error = Some(value),
                "error_description" if description.is_none() => description = Some(value),
                _ => {}
            }
        }
    }
    let value = if state.as_deref() != Some(&callback.expected) {
        Err(Error::Radius(
            0,
            "the sign-in answered for another request".into(),
        ))
    } else if let Some(error) = error {
        Err(Error::Radius(0, description.unwrap_or(error).into_owned()))
    } else {
        code.map(std::borrow::Cow::into_owned)
            .ok_or_else(|| Error::Radius(0, "the sign-in returned no code".into()))
    };
    let succeeded = value.is_ok();
    let Some(sender) = callback.result.lock().await.take() else {
        return (
            StatusCode::GONE,
            [(header::CACHE_CONTROL, "no-store")],
            Html("This Pi sign-in request is no longer active."),
        );
    };
    let _ = sender.send(value);
    let body = if succeeded {
        r#"<!doctype html><meta charset=utf-8><title>Pi</title><body style="font: 15px -apple-system; margin: 4em; text-align: center">Signed in to Radius. You can close this tab and return to Pi."#
    } else {
        "<!doctype html><meta charset=utf-8><title>Pi</title>Pi could not complete sign-in. Return to Pi and try again."
    };
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store")],
        Html(body),
    )
}
