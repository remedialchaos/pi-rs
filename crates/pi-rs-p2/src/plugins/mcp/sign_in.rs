//! Browser sign-ins are transient; tokens and client registrations survive a restart.

use std::{collections::HashMap, sync::Arc, time::Duration};

use pi_rs_chord::{Error, Result, data::ChordError};
use pi_rs_mcp::{
    AbortController, AuthProvider,
    oauth::{
        LoopbackSignInOptions, McpAuthProviderOptions, McpClientMetadataDocuments,
        McpOAuthSettings, McpSignInOptions, McpSignInRedirect, McpSignInStart, OAuthCallback,
        OAuthCallbackPage, OAuthCallbackServer, OAuthChallenge, complete_mcp_sign_in,
        create_mcp_auth_provider, listen_for_loopback_sign_in, response_from_redirect_url,
        start_mcp_sign_in,
    },
};
use serde_json::{Value, json};
use tokio::sync::{Mutex, watch};
use url::Url;
use uuid::Uuid;

use super::{
    config::expand_host_variables, credentials::Credentials, pages, placement::OAuthServer,
};

const TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone)]
struct Flow {
    id: Uuid,
    options: McpSignInOptions,
    authorization: McpSignInRedirect,
    callback: Option<Arc<OAuthCallbackServer>>,
    abort: Arc<AbortController>,
    phase: Phase,
}

#[derive(Clone)]
enum Phase {
    Waiting,
    Completing,
    Failed(String),
}

pub(super) struct SignIns {
    credentials: Credentials,
    client_name: String,
    operations: Mutex<()>,
    flows: Mutex<HashMap<String, Flow>>,
    challenges: std::sync::Mutex<HashMap<(String, String), OAuthChallenge>>,
    changed: watch::Sender<u64>,
    reconnect: tokio::sync::broadcast::Sender<String>,
}

impl SignIns {
    pub fn new(credentials: Credentials, client_name: String) -> Self {
        Self {
            credentials,
            client_name,
            operations: Mutex::new(()),
            flows: Mutex::default(),
            challenges: std::sync::Mutex::default(),
            changed: watch::channel(0).0,
            reconnect: tokio::sync::broadcast::channel(64).0,
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    pub fn credentials_changed(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.reconnect.subscribe()
    }

    pub async fn list(&self) -> Value {
        let flows = self.flows.lock().await;
        let mut entries: Vec<_> = flows.iter().collect();
        entries.sort_by_key(|(left, _)| *left);
        Value::Array(
            entries
                .into_iter()
                .map(|(name, flow)| {
                    let mut view = json!({
                        "id": flow.id.to_string(), "server": name,
                        "via": if flow.callback.is_some() { "loopback" } else { "route" },
                        "status": "waiting",
                    });
                    if let Phase::Failed(error) = &flow.phase {
                        view["status"] = json!("failed");
                        view["error"] = json!(error);
                    }
                    view
                })
                .collect(),
        )
    }

    pub fn provider(self: &Arc<Self>, server: &OAuthServer) -> Result<Arc<dyn AuthProvider>> {
        let settings = settings(server)?;
        let weak = Arc::downgrade(self);
        let key = (server.name.clone(), server.url.clone());
        Ok(create_mcp_auth_provider(McpAuthProviderOptions {
            server_url: server.url.clone(),
            store: self
                .credentials
                .server(&server.name, &server.url)
                .map_err(invalid)?,
            client_name: self.client_name.clone(),
            client_metadata_documents: Some(Arc::new(MetadataDocuments)),
            settings: Arc::new(move || settings.clone()),
            on_challenge: Arc::new(move |challenge| {
                if let Some(sign_ins) = weak.upgrade() {
                    sign_ins
                        .challenges
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .insert(key.clone(), challenge.clone());
                }
            }),
            redirect_url: None,
        }))
    }

    pub async fn begin(
        self: &Arc<Self>,
        server: OAuthServer,
        route: Option<String>,
    ) -> Result<Value> {
        let _operation = self.operations.lock().await;
        let settings = settings(&server)?;
        let store = self
            .credentials
            .server(&server.name, &server.url)
            .map_err(invalid)?;
        let documents: Arc<dyn McpClientMetadataDocuments> = Arc::new(MetadataDocuments);
        // The previous callback owns its port and the two attempts share a PKCE store.
        self.remove(&server.name).await;
        let (redirect_url, callback) = match route.filter(|_| settings.client_id.is_none()) {
            Some(route) => (route, None),
            None => {
                let name = server.name.clone();
                let listener = listen_for_loopback_sign_in(LoopbackSignInOptions {
                    server_url: &server.url,
                    store: store.clone(),
                    settings: &settings,
                    client_metadata_documents: Some(documents.clone()),
                    render_page: Some(Arc::new(move |page| callback_page(&name, page))),
                })
                .await
                .map_err(invalid)?;
                (listener.redirect_url, Some(listener.callback))
            }
        };
        let abort = Arc::new(AbortController::new());
        let options = McpSignInOptions {
            challenge: self
                .challenges
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(&(server.name.clone(), server.url.clone()))
                .cloned(),
            server_url: server.url,
            store,
            settings,
            client_name: self.client_name.clone(),
            client_metadata_documents: Some(documents),
            redirect_url,
            signal: Some(abort.signal()),
        };
        let started = start_mcp_sign_in(options.clone()).await;
        let authorization = match started {
            Ok(McpSignInStart::Redirect(redirect)) => redirect,
            outcome => {
                if let Some(callback) = callback {
                    callback.close().await;
                }
                outcome.map_err(invalid)?;
                let _ = self.reconnect.send(server.name);
                return Ok(json!({"kind":"authorized"}));
            }
        };
        let id = Uuid::new_v4();
        let result = json!({"kind":"redirect","url": authorization.authorization_url});
        let path = Url::parse(&authorization.redirect_url)
            .map_err(invalid)?
            .path()
            .to_owned();
        let flow = Flow {
            id,
            options,
            authorization,
            callback,
            abort,
            phase: Phase::Waiting,
        };
        self.flows
            .lock()
            .await
            .insert(server.name.clone(), flow.clone());
        self.notify();
        let name = server.name;
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let browser = async {
                match &flow.callback {
                    Some(callback) => callback
                        .wait_for_callback(&flow.authorization.state, Some(&path))
                        .await
                        .map_err(|error| error.to_string()),
                    None => std::future::pending().await,
                }
            };
            let response = tokio::select! {
                response = browser => response,
                _ = flow.abort.signal().wait_aborted() => return,
                _ = tokio::time::sleep(TIMEOUT) => Err("The sign-in expired; start it again".to_owned()),
            };
            if let Some(sign_ins) = weak.upgrade() {
                match response {
                    Ok(response) => {
                        let _ = sign_ins.finish(&name, id, response).await;
                    }
                    Err(error) => sign_ins.fail(&name, id, error).await,
                }
            }
        });
        Ok(result)
    }

    pub async fn complete(&self, callback: OAuthCallback, error: Option<String>) -> Result<String> {
        let found = self.flows.lock().await.iter().find_map(|(name, flow)| {
            (flow.callback.is_none()
                && matches!(flow.phase, Phase::Waiting)
                && flow.authorization.state == callback.state)
                .then(|| (name.clone(), flow.id))
        });
        let (name, id) = found.ok_or_else(unknown)?;
        if let Some(error) = error {
            self.fail(&name, id, error.clone()).await;
            return Err(ChordError::new("sign_in", error).into());
        }
        self.finish(&name, id, callback).await?;
        Ok(name)
    }

    pub async fn paste(&self, id: &str, url: &str) -> Result<()> {
        let id = Uuid::parse_str(id).map_err(|_| unknown())?;
        let found = self.flows.lock().await.iter().find_map(|(name, flow)| {
            (flow.id == id && matches!(flow.phase, Phase::Waiting))
                .then(|| (name.clone(), flow.authorization.clone()))
        });
        let (name, authorization) = found.ok_or_else(unknown)?;
        let response = response_from_redirect_url(url, &authorization)
            .map_err(|error| ChordError::new("invalid_url", error.to_string()))?;
        self.finish(&name, id, response).await
    }

    pub async fn cancel(&self, id: &str) {
        let Ok(id) = Uuid::parse_str(id) else { return };
        let _operation = self.operations.lock().await;
        let name = self
            .flows
            .lock()
            .await
            .iter()
            .find_map(|(name, flow)| (flow.id == id).then(|| name.clone()));
        if let Some(name) = name {
            self.remove(&name).await;
        }
    }

    pub async fn sign_out(&self, server: &OAuthServer) -> Result<()> {
        let _operation = self.operations.lock().await;
        self.remove(&server.name).await;
        self.credentials
            .remove(&server.name, &server.url)
            .await
            .map_err(invalid)?;
        let _ = self.reconnect.send(server.name.clone());
        Ok(())
    }

    pub async fn close(&self) {
        let _operation = self.operations.lock().await;
        let flows = std::mem::take(&mut *self.flows.lock().await);
        for (_, flow) in flows {
            stop(flow).await;
        }
    }

    async fn finish(&self, name: &str, id: Uuid, response: OAuthCallback) -> Result<()> {
        let _operation = self.operations.lock().await;
        let flow = {
            let mut flows = self.flows.lock().await;
            let flow = flows
                .get_mut(name)
                .filter(|flow| flow.id == id && matches!(flow.phase, Phase::Waiting))
                .ok_or_else(unknown)?;
            flow.phase = Phase::Completing;
            flow.clone()
        };
        match complete_mcp_sign_in(flow.options, flow.authorization, response).await {
            Ok(()) => {
                self.remove(name).await;
                let _ = self.reconnect.send(name.to_owned());
                Ok(())
            }
            Err(error) => {
                let message = error.to_string();
                self.fail(name, id, message.clone()).await;
                Err(ChordError::new("sign_in", message).into())
            }
        }
    }

    async fn fail(&self, name: &str, id: Uuid, error: String) {
        let flow = {
            let mut flows = self.flows.lock().await;
            flows
                .get_mut(name)
                .filter(|flow| flow.id == id && !matches!(flow.phase, Phase::Failed(_)))
                .map(|flow| {
                    flow.phase = Phase::Failed(error);
                    flow.clone()
                })
        };
        if let Some(flow) = flow {
            stop(flow).await;
            self.notify();
        }
    }

    async fn remove(&self, name: &str) {
        let flow = self.flows.lock().await.remove(name);
        if let Some(flow) = flow {
            stop(flow).await;
            self.notify();
        }
    }

    fn notify(&self) {
        self.changed
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }
}

async fn stop(flow: Flow) {
    flow.abort.abort(None);
    if let Some(callback) = flow.callback {
        callback.close().await;
    }
}

fn settings(server: &OAuthServer) -> Result<McpOAuthSettings> {
    let get = |key| server.config.as_ref().and_then(|config| config.get(key));
    let string = |key| get(key).and_then(Value::as_str).map(str::to_owned);
    Ok(McpOAuthSettings {
        client_id: string("clientId"),
        client_secret: string("clientSecret")
            .map(|secret| expand_host_variables(&secret))
            .transpose()
            .map_err(Error::Invalid)?,
        callback_port: get("callbackPort")
            .and_then(Value::as_u64)
            .and_then(|port| u16::try_from(port).ok()),
        callback_url: string("callbackUrl"),
        scope: string("scope"),
        client_name: string("clientName"),
        client_registration: pi_rs_mcp::oauth::client_settings::client_registration_from_value(
            get("clientRegistration"),
        ),
        auth_server_metadata_url: string("authServerMetadataUrl")
            .map(|url| Url::parse(&url))
            .transpose()
            .map_err(invalid)?,
    })
}

struct MetadataDocuments;

impl McpClientMetadataDocuments for MetadataDocuments {
    fn base_url(&self) -> String {
        "https://pi.dev/oauth".into()
    }

    fn lists_redirect_url(&self, url: &str) -> bool {
        Url::parse(url).is_ok_and(|url| {
            url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "localhost"))
                && url.path() == "/callback"
        })
    }
}

fn callback_page(name: &str, outcome: &OAuthCallbackPage) -> String {
    match outcome {
        OAuthCallbackPage::Ok => {
            pages::result(&format!("Signed in to {name}"), "You may close this page.")
        }
        OAuthCallbackPage::Failed { message, details } => {
            let message = details
                .as_ref()
                .map_or_else(|| message.clone(), |details| format!("{message} {details}"));
            pages::result(&format!("Signing in to {name} failed"), &message)
        }
    }
}

fn unknown() -> Error {
    ChordError::new("not_found", "This sign-in is unknown, finished or expired").into()
}

fn invalid(error: impl std::fmt::Display) -> Error {
    Error::Invalid(error.to_string())
}
