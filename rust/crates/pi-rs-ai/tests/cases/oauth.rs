use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::StatusCode;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, oneshot};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::common::{Server, TestResult, json_reply};
use pi_rs_ai::{
    Error, Result,
    auth::{
        AuthContext, AuthEvent, AuthInteraction, AuthOverrides, AuthPrompt, Credential,
        CredentialStore, InMemoryCredentialStore, LoginOptions, OAuthAuth, OAuthCredential,
        PromptKind, ProviderAuth, resolve_provider_auth,
    },
    oauth::{BuiltinOAuth, Flow},
    providers::env_api_key_auth,
};

#[derive(Default)]
struct Ui {
    responses: Mutex<VecDeque<String>>,
    events: Mutex<Vec<AuthEvent>>,
    callback: Mutex<Option<oneshot::Sender<(StatusCode, StatusCode)>>>,
}
impl Ui {
    fn new(values: &[&str]) -> Self {
        Self {
            responses: Mutex::new(values.iter().map(|s| (*s).into()).collect()),
            ..Self::default()
        }
    }
    fn verify_device_code(&self) -> TestResult {
        assert!(self.events.lock().map_err(|_|"auth UI events lost")?.iter().any(|e|matches!(e,AuthEvent::DeviceCode{user_code,verification_uri,..} if user_code=="ABCD"&&verification_uri.starts_with("https://"))));
        Ok(())
    }
}
#[async_trait]
impl AuthInteraction for Ui {
    async fn prompt(&self, prompt: AuthPrompt) -> Result<String> {
        let input = self
            .responses
            .lock()
            .map_err(|_| Error::OAuth("auth UI lost".into()))?
            .pop_front();
        if let Some(input) = input {
            return Ok(input);
        }
        if matches!(prompt.kind, PromptKind::ManualCode) {
            prompt.signal.cancelled().await;
            Err(Error::Aborted)
        } else {
            Err(Error::OAuth("unexpected auth prompt".into()))
        }
    }
    fn notify(&self, event: AuthEvent) {
        if let AuthEvent::AuthUrl { url, .. } = &event
            && let Ok(url) = Url::parse(url)
            && let Ok(mut callback) = self.callback.lock()
            && let Some(sender) = callback.take()
        {
            tokio::spawn(async move {
                let request = async {
                    let redirect = url
                        .query_pairs()
                        .find(|(key, _)| key == "redirect_uri" || key == "callback_url")
                        .map(|(_, v)| v)
                        .ok_or("no redirect URI")?;
                    let expected = url
                        .query_pairs()
                        .find(|(k, _)| k == "state")
                        .map(|(_, v)| v.into_owned());
                    let mut redirect = Url::parse(&redirect)?;
                    let client = reqwest::Client::builder()
                        .no_proxy()
                        .timeout(Duration::from_secs(3))
                        .build()?;
                    let wrong = if expected.is_some() {
                        let mut wrong = redirect.clone();
                        wrong.query_pairs_mut().extend_pairs([
                            ("code", "bad"),
                            ("client_id", "issued"),
                            ("state", "bad-state"),
                        ]);
                        client.get(wrong).send().await?.status()
                    } else {
                        StatusCode::BAD_REQUEST
                    };
                    redirect
                        .query_pairs_mut()
                        .extend_pairs([("code", "test-code"), ("client_id", "issued")]);
                    if let Some(state) = expected {
                        redirect.query_pairs_mut().append_pair("state", &state);
                    }
                    Ok::<_, Box<dyn std::error::Error>>((
                        wrong,
                        client.get(redirect).send().await?.status(),
                    ))
                }
                .await;
                let _ = sender.send(request.unwrap_or((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    StatusCode::INTERNAL_SERVER_ERROR,
                )));
            });
        }
        if let Ok(mut events) = self.events.lock() {
            events.push(event);
        }
    }
}
fn oauth(flow: Flow, server: &Server, hosts: &[&str]) -> TestResult<BuiltinOAuth> {
    let endpoint = Url::parse(&server.url)?;
    let mut auth = BuiltinOAuth::new(flow)?
        .with_client(reqwest::Client::builder().no_proxy().build()?)
        .with_callback_address("127.0.0.1", 0);
    for host in hosts {
        auth = auth.with_endpoint(*host, endpoint.clone());
    }
    Ok(auth)
}
fn token(access: &str) -> Value {
    json!({"access_token":access,"refresh_token":"fixture-refresh","expires_in":3600})
}
fn device(complete: bool) -> Value {
    let mut v = json!({"device_code":"device","user_code":"ABCD","verification_uri":"https://example.test/device","interval":1,"expires_in":30});
    if complete {
        v["verification_uri_complete"] = json!("https://example.test/device?code=ABCD");
    }
    v
}
fn form(body: &[u8]) -> indexmap::IndexMap<String, String> {
    url::form_urlencoded::parse(body).into_owned().collect()
}
#[tokio::test]
async fn anthropic_copy_code_exchanges_verifier_and_rotates_refresh_token() -> TestResult {
    let server = Server::start(vec![
        json_reply(token("sk-ant-oat-fixture")),
        json_reply(token("rotated")),
    ])
    .await?;
    let flow = oauth(Flow::Anthropic, &server, &["platform.claude.com"])?;
    let ui = Ui::new(&["copy_code", "test-code"]);
    let creds = flow.login(&ui, &LoginOptions::default()).await?;
    assert_eq!(creds.access, "sk-ant-oat-fixture");
    assert_eq!(
        flow.refresh(creds, CancellationToken::new()).await?.access,
        "rotated"
    );
    let wire = server.drain()?;
    assert_eq!(wire[0].path, "/v1/oauth/token");
    let body = wire[0].json()?;
    let verifier = body["code_verifier"]
        .as_str()
        .ok_or("missing PKCE verifier")?;
    assert_eq!(
        body["redirect_uri"],
        "https://platform.claude.com/oauth/code/callback"
    );
    assert_eq!(body["code"], "test-code");
    assert_eq!(body["state"], verifier);
    let events = ui.events.lock().map_err(|_| "auth UI lock lost")?;
    let url = events
        .iter()
        .find_map(|e| match e {
            AuthEvent::AuthUrl { url, .. } => Some(url),
            _ => None,
        })
        .ok_or("no auth URL")?;
    let url = Url::parse(url)?;
    let challenge = url
        .query_pairs()
        .find(|(k, _)| k == "code_challenge")
        .ok_or("missing PKCE challenge")?
        .1;
    assert_eq!(
        challenge,
        URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
    );
    assert_eq!(wire[1].json()?["refresh_token"], "fixture-refresh");
    Ok(())
}
#[tokio::test]
async fn codex_device_exchanges_code_and_derives_account_id() -> TestResult {
    let claim = URL_SAFE_NO_PAD
        .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"account-fixture"}}"#);
    let server = Server::start(vec![
        json_reply(json!({"device_auth_id":"device","user_code":"ABCD","interval":"0"})),
        json_reply(json!({"authorization_code":"code","code_verifier":"verify"})),
        json_reply(token(&format!("x.{claim}.y"))),
    ])
    .await?;
    let flow = oauth(Flow::Codex, &server, &["auth.openai.com"])?;
    let ui = Ui::new(&["device_code"]);
    let creds = flow.login(&ui, &LoginOptions::default()).await?;
    ui.verify_device_code()?;
    assert_eq!(creds.details["accountId"], "account-fixture");
    let requests = server.drain()?;
    assert_eq!(requests[0].path, "/api/accounts/deviceauth/usercode");
    assert_eq!(requests[1].json()?["device_auth_id"], "device");
    assert_eq!(
        form(&requests[2].body)["redirect_uri"],
        "https://auth.openai.com/deviceauth/callback"
    );
    assert_eq!(form(&requests[2].body)["code_verifier"], "verify");
    Ok(())
}
#[tokio::test]
async fn chatgpt_registered_client_requires_state_and_direct_grant() -> TestResult {
    let mut output = token("chatgpt-token");
    output["id_token"] = json!("unused");
    output["scope"] = json!("openid chatgpt.tokens.use.direct");
    let server = Server::start(vec![json_reply(output)]).await?;
    let flow = oauth(Flow::ChatGpt, &server, &["auth.openai.com"])?;
    let ui = Ui::default();
    let (send, rx) = oneshot::channel();
    *ui.callback.lock().map_err(|_| "auth UI lock lost")? = Some(send);
    let opts = LoginOptions {
        get_device_id: Some(Arc::new(|| "00000000-0000-4000-8000-000000000001".into())),
        ..Default::default()
    };
    let creds = tokio::time::timeout(Duration::from_secs(5), flow.login(&ui, &opts)).await??;
    assert_eq!(rx.await?, (StatusCode::BAD_REQUEST, StatusCode::OK));
    assert_eq!(creds.details["clientId"], "issued");
    let req = server.drain()?;
    let fields = form(&req[0].body);
    assert_eq!(req[0].path, "/api/accounts/oauth/token");
    assert_eq!(fields["client_id"], "issued");
    assert_eq!(fields["code"], "test-code");
    Ok(())
}
#[tokio::test]
async fn copilot_mints_proxied_token_and_filters_model_listing() -> TestResult {
    let server=Server::start(vec![json_reply(device(false)),json_reply(json!({"access_token":"github-token"})),json_reply(json!({"token":"tid=test;proxy-ep=proxy.fixture.test","expires_at":4000000000u64})),
        json_reply(json!({"data":[{"id":"enabled","model_picker_enabled":true},{"id":"disabled","model_picker_enabled":true,"policy":{"state":"disabled"}},{"id":"no-tools","model_picker_enabled":true,"capabilities":{"supports":{"tool_calls":false}}}]}))]).await?;
    let auth = oauth(
        Flow::Copilot,
        &server,
        &["github.com", "api.github.com", "api.fixture.test"],
    )?;
    let ui = Ui::new(&[""]);
    let creds = auth.login(&ui, &LoginOptions::default()).await?;
    ui.verify_device_code()?;
    assert_eq!(creds.refresh, "github-token");
    assert_eq!(creds.details["availableModelIds"], json!(["enabled"]));
    assert_eq!(
        auth.to_auth(&creds).await?.base_url.as_deref(),
        Some("https://api.fixture.test")
    );
    let requests = server.drain()?;
    assert_eq!(requests[2].headers["authorization"], "Bearer github-token");
    assert_eq!(requests[3].path, "/models");
    assert_eq!(requests[3].headers["x-github-api-version"], "2026-06-01");
    Ok(())
}
#[tokio::test]
async fn openrouter_issues_permanent_api_key_from_pkce() -> TestResult {
    let server = Server::start(vec![json_reply(json!({"key":"router-key"}))]).await?;
    let auth = oauth(Flow::OpenRouter, &server, &["openrouter.ai"])?;
    let ui = Ui::new(&["test-code"]);
    let c = auth.login(&ui, &LoginOptions::default()).await?;
    assert_eq!(c.expires, 9_007_199_254_740_991);
    assert!(c.refresh.is_empty());
    assert_eq!(
        auth.refresh(c, CancellationToken::new()).await?.access,
        "router-key"
    );
    let calls = server.drain()?;
    let body = calls[0].json()?;
    assert_eq!(calls[0].path, "/api/v1/auth/keys");
    assert_eq!(body["code"], "test-code");
    assert_eq!(body["code_challenge_method"], "S256");
    Ok(())
}
#[tokio::test]
async fn kimi_oauth_uses_bearer_header_instead_of_api_key() -> TestResult {
    let server = Server::start(vec![
        json_reply(device(true)),
        json_reply(token("kimi-token")),
    ])
    .await?;
    let auth = oauth(Flow::Kimi, &server, &["auth.kimi.com"])?;
    let ui = Ui::default();
    let creds = auth.login(&ui, &LoginOptions::default()).await?;
    ui.verify_device_code()?;
    let auth = auth.to_auth(&creds).await?;
    assert!(auth.api_key.is_none());
    assert_eq!(
        auth.headers.ok_or("missing Kimi bearer header")?["Authorization"].as_deref(),
        Some("Bearer kimi-token")
    );
    let requests = server.drain()?;
    assert_eq!(requests[0].path, "/api/oauth/device_authorization");
    assert_eq!(form(&requests[1].body)["device_code"], "device");
    Ok(())
}
#[tokio::test]
async fn meta_exchanges_identity_for_inference_key_and_remints_it() -> TestResult {
    let server = Server::start(vec![
        json_reply(device(true)),
        json_reply(json!({"access_token":"meta-identity"})),
        json_reply(json!({"api_key":"meta-key"})),
        json_reply(json!({"api_key":"meta-key2"})),
    ])
    .await?;
    let flow = oauth(Flow::Meta, &server, &["auth.meta.com", "api.meta.ai"])?;
    let ui = Ui::default();
    let c = flow.login(&ui, &LoginOptions::default()).await?;
    ui.verify_device_code()?;
    assert_eq!(c.access, "meta-key");
    assert_eq!(c.refresh, "meta-identity");
    assert_eq!(
        flow.refresh(c, CancellationToken::new()).await?.access,
        "meta-key2"
    );
    let requests = server.drain()?;
    assert_eq!(requests[2].path, "/muse-code/key");
    assert_eq!(requests[3].headers["authorization"], "Bearer meta-identity");
    Ok(())
}
#[tokio::test]
async fn xai_device_refresh_preserves_unrotated_refresh_token() -> TestResult {
    let server = Server::start(vec![
        json_reply(device(true)),
        json_reply(token("grok")),
        json_reply(json!({"access_token":"grok-next"})),
    ])
    .await?;
    let flow = oauth(Flow::Xai, &server, &["auth.x.ai"])?;
    let ui = Ui::default();
    let c = flow.login(&ui, &LoginOptions::default()).await?;
    ui.verify_device_code()?;
    assert_eq!(c.access, "grok");
    let refresh = flow.refresh(c, CancellationToken::new()).await?;
    assert_eq!(refresh.refresh, "fixture-refresh");
    assert_eq!(refresh.access, "grok-next");
    let req = server.drain()?;
    assert_eq!(req[0].path, "/oauth2/device/code");
    assert_eq!(form(&req[0].body)["referrer"], "pi");
    assert_eq!(form(&req[2].body)["grant_type"], "refresh_token");
    Ok(())
}
#[tokio::test]
async fn radius_uses_gateway_token_routes_and_copies_scope() -> TestResult {
    let mut response = token("radius-key");
    response["scope"] = json!("gateway");
    let server = Server::start(vec![
        json_reply(device(false)),
        json_reply(response.clone()),
        json_reply(response),
    ])
    .await?;
    let auth = oauth(Flow::Radius, &server, &[])?
        .with_radius_gateway("Fixture", Url::parse(&server.url)?)?;
    let ui = Ui::new(&["device-code"]);
    let c = auth.login(&ui, &LoginOptions::default()).await?;
    ui.verify_device_code()?;
    assert_eq!(c.details["scope"], "gateway");
    assert_eq!(
        auth.refresh(c, CancellationToken::new()).await?.access,
        "radius-key"
    );
    let requests = server.drain()?;
    assert_eq!(requests[0].path, "/v1/oauth/device");
    assert_eq!(requests[1].path, "/v1/oauth/token");
    assert_eq!(form(&requests[1].body)["client_id"], "pi-gateway");
    Ok(())
}

struct Ambient;
#[async_trait]
impl AuthContext for Ambient {
    async fn env(&self, _: &str) -> Option<String> {
        Some("ambient-key".into())
    }
    async fn file_exists(&self, _: &str) -> bool {
        false
    }
}
#[derive(Default)]
struct Rotation {
    count: AtomicUsize,
    started: Notify,
    allow: Notify,
}
#[async_trait]
impl OAuthAuth for Rotation {
    fn name(&self) -> &str {
        "Rotation fixture"
    }
    async fn login(&self, _: &dyn AuthInteraction, _: &LoginOptions) -> Result<OAuthCredential> {
        Err(Error::OAuth("unexpected login".into()))
    }
    async fn refresh(
        &self,
        previous: OAuthCredential,
        _: CancellationToken,
    ) -> Result<OAuthCredential> {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        self.allow.notified().await;
        if previous.refresh != "old" {
            return Err(Error::OAuth("refresh token reused".into()));
        }
        Ok(OAuthCredential {
            access: "new-access".into(),
            refresh: "new-refresh".into(),
            expires: u64::MAX,
            details: Map::new(),
        })
    }
}
#[tokio::test]
async fn cancelled_request_does_not_discard_rotated_credentials_or_double_refresh() -> TestResult {
    let store: Arc<dyn CredentialStore> = Arc::new(InMemoryCredentialStore::default());
    let token = OAuthCredential {
        access: "expired".into(),
        refresh: "old".into(),
        expires: 0,
        details: Map::new(),
    };
    store
        .modify(
            "fixture",
            Box::new(|_| Box::pin(async { Ok(Some(Credential::OAuth(token))) })),
            &CancellationToken::new(),
        )
        .await?;
    let rotator = Arc::new(Rotation::default());
    let auth = ProviderAuth {
        api_key: Some(env_api_key_auth("Fixture", ["FIXTURE_KEY"])),
        oauth: Some(rotator.clone()),
    };
    let cancel = CancellationToken::new();
    let options = AuthOverrides {
        signal: cancel.clone(),
        ..Default::default()
    };
    let first = tokio::spawn({
        let auth = auth.clone();
        let store = store.clone();
        async move { resolve_provider_auth("fixture", &auth, store, &Ambient, &options).await }
    });
    tokio::time::timeout(Duration::from_secs(3), rotator.started.notified()).await?;
    let second = tokio::spawn({
        let auth = auth.clone();
        let store = store.clone();
        async move {
            resolve_provider_auth("fixture", &auth, store, &Ambient, &AuthOverrides::default())
                .await
        }
    });
    cancel.cancel();
    assert!(matches!(first.await?, Err(Error::Aborted)));
    rotator.allow.notify_one();
    let current = tokio::time::timeout(Duration::from_secs(3), second)
        .await???
        .ok_or("no resolved credential")?;
    assert_eq!(current.auth.api_key.as_deref(), Some("new-access"));
    assert_eq!(rotator.count.load(Ordering::SeqCst), 1);
    let stored = store
        .read("fixture", &CancellationToken::new())
        .await?
        .ok_or("lost credential")?;
    assert_eq!(serde_json::to_value(stored)?["type"], "oauth");
    Ok(())
}
