//! `packages/mcp/test/auth-provider.test.ts`.

use std::sync::Arc;
use std::sync::Mutex;

use serde_json::{Map, Value, json};
use url::Url;

use pi_rs_mcp::oauth::{
    ClientRegistration, McpAuthProviderOptions, McpClientMetadataDocuments, McpOAuthSettings,
    McpOAuthState, McpSignInOptions, McpSignInRedirect, McpSignInStart, OAuthChallenge,
    OAuthClientInformation, complete_mcp_sign_in, create_mcp_auth_provider, start_mcp_sign_in,
};
use pi_rs_mcp::{McpFetch, McpFetchError, McpFetchRequest, UnauthorizedContext};
use support::{fetch_response, stream_of_chunks};

#[path = "support/mod.rs"]
#[expect(
    dead_code,
    reason = "shared test helpers: each integration-test binary uses a different subset"
)]
mod support;

use support::{
    AuthorizationServer, AuthorizationServerOptions, OAuthCallbackResponse, close_servers,
    form_get, memory_store, start_authorization_server, suite_turn,
};

/// The upstream error class's message, as the port's `AuthorizationRequired` carries it.
const AUTHORIZATION_REQUIRED: &str = "MCP OAuth authorization requires user interaction";

const ROUTE: &str = "https://app.example/mcp/callback";

/// The sign-in options each test signs in with, bound to one server and store.
#[derive(Clone)]
struct Options {
    server: Arc<AuthorizationServer>,
    store: Arc<dyn pi_rs_mcp::oauth::McpOAuthServerStore>,
    redirect_url: String,
    documents: Option<Arc<dyn McpClientMetadataDocuments>>,
}

fn sign_in_options(server: &Arc<AuthorizationServer>, redirect_url: &str) -> Options {
    Options {
        server: server.clone(),
        store: memory_store(),
        redirect_url: redirect_url.to_string(),
        documents: None,
    }
}

fn options_of(options: &Options, settings: McpOAuthSettings) -> McpSignInOptions {
    McpSignInOptions {
        server_url: options.server.server_url.clone(),
        store: options.store.clone(),
        settings,
        client_name: "app".to_string(),
        client_metadata_documents: options.documents.clone(),
        redirect_url: options.redirect_url.clone(),
        challenge: None,
        signal: None,
    }
}

/// The `redirect` result of a start.
async fn start_redirect(options: &Options, settings: McpOAuthSettings) -> McpSignInRedirect {
    match start_mcp_sign_in(options_of(options, settings))
        .await
        .expect("startMcpSignIn")
    {
        McpSignInStart::Authorized => panic!("Expected the browser step"),
        McpSignInStart::Redirect(redirect) => redirect,
    }
}

/// Starts, follows the browser step, and completes.
async fn sign_in(options: &Options, settings: McpOAuthSettings) -> McpSignInRedirect {
    let authorization = start_redirect(options, settings.clone()).await;
    let location = support::authorize_in_browser(&authorization.authorization_url).await;
    complete_mcp_sign_in(
        options_of(options, settings),
        authorization.clone(),
        OAuthCallbackResponse::from_url(&location).into_callback(),
    )
    .await
    .expect("completeMcpSignIn");
    authorization
}

/// Starts with a step-up challenge hanging over the stored tokens.
async fn start_step_up(options: &Options, settings: McpOAuthSettings) -> McpSignInRedirect {
    match start_mcp_sign_in(McpSignInOptions {
        challenge: Some(OAuthChallenge {
            error: Some("insufficient_scope".to_string()),
            scope: Some("more".to_string()),
            ..Default::default()
        }),
        ..options_of(options, settings)
    })
    .await
    .expect("startMcpSignIn")
    {
        McpSignInStart::Authorized => panic!("Expected the browser step"),
        McpSignInStart::Redirect(redirect) => redirect,
    }
}

/// What a connection's request reports after its token was rejected.
fn unauthorized(server: &Arc<AuthorizationServer>, token: &str) -> UnauthorizedContext {
    UnauthorizedContext {
        status: 401,
        headers: pi_rs_mcp::header_map([("www-authenticate", "Bearer")]),
        server_url: Url::parse(&server.server_url)
            .unwrap_or_else(|_| Url::parse("http://127.0.0.1/").expect("stand-in URL")),
        fetch: McpFetch::platform(),
        token: Some(token.to_string()),
    }
}

/// The connection auth provider for a sign-in's store and settings.
fn provider(
    options: &Options,
    settings: McpOAuthSettings,
) -> Arc<dyn pi_rs_mcp::oauth::McpAuthProvider> {
    create_mcp_auth_provider(McpAuthProviderOptions {
        server_url: options.server.server_url.clone(),
        store: options.store.clone(),
        client_name: "app".to_string(),
        client_metadata_documents: options.documents.clone(),
        settings: Arc::new(move || settings.clone()),
        on_challenge: Arc::new(|_challenge| {}),
        redirect_url: Some(options.redirect_url.clone()),
    })
}

/// Documents that list `/callback` on 127.0.0.1, like pi's on pi.dev.
struct LoopbackDocuments;

impl McpClientMetadataDocuments for LoopbackDocuments {
    fn base_url(&self) -> String {
        "https://app.example/oauth".to_string()
    }

    fn lists_redirect_url(&self, redirect_url: &str) -> bool {
        match Url::parse(redirect_url) {
            Ok(url) => url.host_str() == Some("127.0.0.1") && url.path() == "/callback",
            Err(_) => false,
        }
    }
}

/// Documents that list only the application's https callback route.
struct RouteDocuments;

impl McpClientMetadataDocuments for RouteDocuments {
    fn base_url(&self) -> String {
        "https://app.example/oauth".to_string()
    }

    fn lists_redirect_url(&self, redirect_url: &str) -> bool {
        redirect_url == ROUTE
    }
}

#[tokio::test]
async fn refreshes_a_rejected_token() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions::default()).await;
    let options = sign_in_options(&server, ROUTE);
    sign_in(&options, McpOAuthSettings::default()).await;
    let auth = provider(&options, McpOAuthSettings::default());

    pi_rs_mcp::AuthProvider::on_unauthorized(auth.as_ref(), unauthorized(&server, "access-1"))
        .await
        .expect("refresh");
    let token = pi_rs_mcp::AuthProvider::token(auth.as_ref())
        .await
        .expect("token");
    assert_eq!(token.as_deref(), Some("access-2"));
    let grants = server.token_requests.lock().unwrap().clone();
    assert_eq!(form_get(&grants[1], "client_id"), "client-1");
    close_servers();
}

#[tokio::test]
async fn leaves_a_pending_sign_in_usable_when_the_refresh_fails() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions::default()).await;
    let options = sign_in_options(&server, ROUTE);
    sign_in(&options, McpOAuthSettings::default()).await;
    // A step-up keeps the tokens while the user is in the browser.
    let authorization = start_step_up(&options, McpOAuthSettings::default()).await;
    let pending = options
        .store
        .load()
        .await
        .expect("stored state")
        .expect("the sign-in stored its state");

    server.revoke_refresh_tokens();
    let auth = provider(&options, McpOAuthSettings::default());
    let error =
        pi_rs_mcp::AuthProvider::on_unauthorized(auth.as_ref(), unauthorized(&server, "access-1"))
            .await
            .expect_err("refresh fails");
    assert_eq!(error.to_string(), AUTHORIZATION_REQUIRED);

    let state = options
        .store
        .load()
        .await
        .expect("stored state")
        .expect("the sign-in stored its state");
    assert_eq!(state.oauth_state, pending.oauth_state);
    assert_eq!(state.code_verifier, pending.code_verifier);
    assert_eq!(server.registrations.lock().unwrap().len(), 1);

    let location = support::authorize_in_browser(&authorization.authorization_url).await;
    complete_mcp_sign_in(
        options_of(&options, McpOAuthSettings::default()),
        authorization,
        OAuthCallbackResponse::from_url(&location).into_callback(),
    )
    .await
    .expect("completeMcpSignIn");
    let state = options
        .store
        .load()
        .await
        .expect("stored state")
        .expect("the sign-in stored its state");
    assert_eq!(
        state
            .tokens
            .as_ref()
            .map(|tokens| tokens.access_token.as_str()),
        Some("access-2")
    );
    close_servers();
}

#[tokio::test]
async fn does_not_register_a_client_to_refresh() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions {
        cimd: true,
        iss_parameter: true,
    })
    .await;
    let mut options = sign_in_options(&server, "http://127.0.0.1:1/callback");
    options.documents = Some(Arc::new(LoopbackDocuments));
    let settings = McpOAuthSettings::default();
    sign_in(&options, settings).await;
    let state = options
        .store
        .load()
        .await
        .expect("stored state")
        .expect("the sign-in stored its state");
    assert!(state.client_information.is_none());

    // Without the documents, the tokens have no client to refresh them with.
    let mut bare = options.clone();
    bare.documents = None;
    let auth = provider(&bare, McpOAuthSettings::default());
    let error =
        pi_rs_mcp::AuthProvider::on_unauthorized(auth.as_ref(), unauthorized(&server, "access-1"))
            .await
            .expect_err("no client to refresh with");
    assert_eq!(error.to_string(), AUTHORIZATION_REQUIRED);
    assert!(server.registrations.lock().unwrap().is_empty());
    assert_eq!(server.authorizations.lock().unwrap().len(), 1);
    close_servers();
}

#[tokio::test]
async fn refreshes_the_tokens_of_a_document_that_lists_only_the_redirect_uri_passed() {
    let _turn = suite_turn().await;
    for client_registration in [ClientRegistration::Auto, ClientRegistration::Cimd] {
        let server = start_authorization_server(AuthorizationServerOptions {
            cimd: true,
            iss_parameter: true,
        })
        .await;
        let mut options = sign_in_options(&server, ROUTE);
        options.documents = Some(Arc::new(RouteDocuments));
        let settings = McpOAuthSettings {
            client_registration: Some(client_registration),
            ..Default::default()
        };
        sign_in(&options, settings.clone()).await;
        let state = options
            .store
            .load()
            .await
            .expect("stored state")
            .expect("the sign-in stored its state");
        assert!(state.client_information.is_none());

        let auth = provider(&options, settings);
        pi_rs_mcp::AuthProvider::on_unauthorized(auth.as_ref(), unauthorized(&server, "access-1"))
            .await
            .expect("refresh");
        let token = pi_rs_mcp::AuthProvider::token(auth.as_ref())
            .await
            .expect("token");
        assert_eq!(token.as_deref(), Some("access-2"));
        let grants = server.token_requests.lock().unwrap().clone();
        assert_eq!(
            form_get(&grants[1], "client_id"),
            "https://app.example/oauth/client.json"
        );
        assert_eq!(server.authorizations.lock().unwrap().len(), 1);
        close_servers();
    }
}

/// Signed in, with a step-up sign-in waiting for the browser, which keeps the tokens meanwhile.
async fn pending_sign_in() -> (
    Arc<AuthorizationServer>,
    Options,
    pi_rs_mcp::oauth::McpOAuthState,
) {
    let server = start_authorization_server(AuthorizationServerOptions::default()).await;
    let options = sign_in_options(&server, ROUTE);
    sign_in(&options, McpOAuthSettings::default()).await;
    start_step_up(&options, McpOAuthSettings::default()).await;
    let pending = options
        .store
        .load()
        .await
        .expect("stored state")
        .expect("the first sign-in stored state");
    (server, options, pending)
}

#[tokio::test]
async fn drops_a_rejected_client_and_its_tokens_but_keeps_the_pending_sign_in() {
    let _turn = suite_turn().await;
    let (server, options, pending) = pending_sign_in().await;
    server.fail_next_refresh("invalid_client", None);
    let auth = provider(&options, McpOAuthSettings::default());
    let error =
        pi_rs_mcp::AuthProvider::on_unauthorized(auth.as_ref(), unauthorized(&server, "access-1"))
            .await
            .expect_err("client rejected");
    assert_eq!(error.to_string(), AUTHORIZATION_REQUIRED);
    let state = options
        .store
        .load()
        .await
        .expect("stored state")
        .expect("the sign-in stored its state");
    assert_eq!(state.oauth_state, pending.oauth_state);
    assert_eq!(state.code_verifier, pending.code_verifier);
    assert!(state.client_information.is_none());
    assert!(state.tokens.is_none());
    assert_eq!(server.registrations.lock().unwrap().len(), 1);
    close_servers();
}

#[tokio::test]
async fn keeps_a_client_stored_while_the_rejected_refresh_ran() {
    let _turn = suite_turn().await;
    let (server, options, _pending) = pending_sign_in().await;
    // Another sign-in completes with a new client while the server rejects the old one.
    let store = options.store.clone();
    server.fail_next_refresh(
        "invalid_client",
        Some(Arc::new(move || {
            let store = store.clone();
            Box::pin(async move {
                let state = store.load().await;
                if let Ok(Some(state)) = state {
                    let mut extra = Map::new();
                    extra.insert(
                        "client_id".to_string(),
                        Value::String("client-new".to_string()),
                    );
                    extra.insert("redirect_uris".to_string(), json!([ROUTE]));
                    let _ = store
                        .save(McpOAuthState {
                            client_information: Some(OAuthClientInformation {
                                client_id: "client-new".to_string(),
                                client_secret: None,
                                client_id_issued_at: None,
                                client_secret_expires_at: None,
                                extra,
                            }),
                            ..state
                        })
                        .await;
                }
            })
        })),
    );
    let auth = provider(&options, McpOAuthSettings::default());
    pi_rs_mcp::AuthProvider::on_unauthorized(auth.as_ref(), unauthorized(&server, "access-1"))
        .await
        .expect("refresh with the stored client");
    let state = options
        .store
        .load()
        .await
        .expect("stored state")
        .expect("the sign-in stored its state");
    assert_eq!(
        state
            .client_information
            .as_ref()
            .map(|client| client.client_id.as_str()),
        Some("client-new")
    );
    let grants = server.token_requests.lock().unwrap().clone();
    assert_eq!(form_get(grants.last().unwrap(), "client_id"), "client-new");
    let token = pi_rs_mcp::AuthProvider::token(auth.as_ref())
        .await
        .expect("token");
    assert_eq!(token.as_deref(), Some("access-2"));
    close_servers();
}

#[tokio::test]
async fn keeps_the_tokens_and_the_pending_sign_in_after_a_server_error() {
    let _turn = suite_turn().await;
    let (server, options, pending) = pending_sign_in().await;
    server.fail_next_refresh("server_error", None);
    let auth = provider(&options, McpOAuthSettings::default());
    let error =
        pi_rs_mcp::AuthProvider::on_unauthorized(auth.as_ref(), unauthorized(&server, "access-1"))
            .await
            .expect_err("redirect needed");
    assert_eq!(error.to_string(), AUTHORIZATION_REQUIRED);
    let state = options
        .store
        .load()
        .await
        .expect("stored state")
        .expect("the sign-in stored its state");
    assert_eq!(state.oauth_state, pending.oauth_state);
    assert_eq!(state.code_verifier, pending.code_verifier);
    assert_eq!(
        state
            .tokens
            .as_ref()
            .map(|tokens| tokens.access_token.as_str()),
        Some("access-1")
    );
    close_servers();
}

#[tokio::test]
async fn keeps_the_tokens_and_the_pending_sign_in_after_a_network_error() {
    let _turn = suite_turn().await;
    let (server, options, pending) = pending_sign_in().await;
    let offline = McpFetch::from_fn(|request: McpFetchRequest| {
        Box::pin(async move {
            if request.url.ends_with("/token") {
                Err(McpFetchError::Network("fetch failed".to_string()))
            } else {
                Ok(fetch_response(
                    200,
                    &[("content-type", "application/json")],
                    stream_of_chunks(Vec::new()),
                ))
            }
        })
    });
    let mut context = unauthorized(&server, "access-1");
    context.fetch = offline;
    let auth = provider(&options, McpOAuthSettings::default());
    let error = pi_rs_mcp::AuthProvider::on_unauthorized(auth.as_ref(), context)
        .await
        .expect_err("network failure reaches the caller");
    assert_eq!(error.to_string(), AUTHORIZATION_REQUIRED);
    let state = options
        .store
        .load()
        .await
        .expect("stored state")
        .expect("the sign-in stored its state");
    assert_eq!(state.oauth_state, pending.oauth_state);
    assert_eq!(state.code_verifier, pending.code_verifier);
    assert_eq!(
        state
            .tokens
            .as_ref()
            .map(|tokens| tokens.access_token.as_str()),
        Some("access-1")
    );
    close_servers();
}

/// The auth provider reports the server's challenge to sign-in.
#[tokio::test]
async fn reports_the_servers_challenge_to_the_caller() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions::default()).await;
    let options = sign_in_options(&server, ROUTE);
    let seen: Arc<Mutex<Vec<pi_rs_mcp::oauth::OAuthChallenge>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_for_provider = seen.clone();
    let auth = create_mcp_auth_provider(McpAuthProviderOptions {
        server_url: options.server.server_url.clone(),
        store: options.store.clone(),
        client_name: "app".to_string(),
        client_metadata_documents: None,
        settings: Arc::new(McpOAuthSettings::default),
        on_challenge: Arc::new(move |challenge| {
            seen_for_provider.lock().unwrap().push(challenge.clone());
        }),
        redirect_url: None,
    });
    let mut context = unauthorized(&server, "access-1");
    context.headers = pi_rs_mcp::header_map([(
        "www-authenticate",
        "Bearer error=\"insufficient_scope\", scope=\"admin\"",
    )]);
    let error = pi_rs_mcp::AuthProvider::on_unauthorized(auth.as_ref(), context)
        .await
        .expect_err("more scope needs a sign-in");
    assert_eq!(error.to_string(), AUTHORIZATION_REQUIRED);
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].error.as_deref(), Some("insufficient_scope"));
    assert_eq!(seen[0].scope.as_deref(), Some("admin"));
    close_servers();
}
