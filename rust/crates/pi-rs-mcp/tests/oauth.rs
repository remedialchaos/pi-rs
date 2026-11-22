//! `packages/mcp/test/oauth.test.ts`.

use std::sync::{Arc, Mutex, atomic::AtomicUsize};

use serde_json::{Value, json};
use url::Url;

use pi_rs_mcp::oauth::{
    AuthorizationServerMetadata, McpOAuthStateStore as _, MemoryOAuthStateStore, OAuthCallbackPage,
    OAuthClientInformation, OAuthClientMetadata, OAuthClientMetadataDocument, OAuthClientProvider,
    OAuthDiscoveryState, OAuthError, OAuthTokens, StoreFuture, adapt_oauth_provider, authorize_mcp,
    discover_authorization_server_metadata, register_client, start_authorization,
};

#[path = "support/mod.rs"]
#[expect(
    dead_code,
    reason = "shared test helpers: each integration-test binary uses a different subset"
)]
mod support;

use support::{TestBody, TestResponse, close_servers, form_get, suite_turn};

/// Closes the callback server the tests hold behind an `Arc`.
async fn callback_server_close(callback: Arc<pi_rs_mcp::oauth::OAuthCallbackServer>) {
    callback.close().await;
}

/// The upstream `TestOAuthProvider`: an in-memory provider the tests drive directly. Its fields
/// sit in shared cells so the trait methods can mutate them through `&self`.
#[derive(Clone)]
struct Shared {
    client: Option<OAuthClientInformation>,
    token_set: Option<OAuthTokens>,
    verifier: Option<String>,
    discovery: Option<OAuthDiscoveryState>,
    authorization_url: Option<Url>,
}

/// The upstream `TestOAuthProvider`.
struct TestOAuthProvider {
    redirect_url: String,
    shared: Arc<Mutex<Shared>>,
}

impl TestOAuthProvider {
    fn new(redirect_url: &str) -> Arc<Self> {
        Arc::new(Self {
            redirect_url: redirect_url.to_string(),
            shared: Arc::new(Mutex::new(Shared {
                client: None,
                token_set: None,
                verifier: None,
                discovery: None,
                authorization_url: None,
            })),
        })
    }

    fn shared(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap()
    }

    fn set_token_set(&self, tokens: Option<OAuthTokens>) {
        self.shared().token_set = tokens;
    }
}

impl OAuthClientProvider for TestOAuthProvider {
    fn redirect_url(&self) -> String {
        self.redirect_url.clone()
    }

    fn client_metadata(&self) -> OAuthClientMetadata {
        OAuthClientMetadata {
            redirect_uris: Some(vec![self.redirect_url.clone()]),
            client_name: Some("pi-mcp-test".to_string()),
            grant_types: Some(vec![
                "authorization_code".to_string(),
                "refresh_token".to_string(),
            ]),
            response_types: Some(vec!["code".to_string()]),
            token_endpoint_auth_method: Some("none".to_string()),
            ..Default::default()
        }
    }

    fn client_metadata_document(
        &self,
        _metadata: Option<&AuthorizationServerMetadata>,
    ) -> Result<Option<OAuthClientMetadataDocument>, String> {
        Ok(None)
    }

    fn state(&self) -> StoreFuture<'_, Option<String>> {
        Box::pin(async { Ok(Some("expected-state".to_string())) })
    }

    fn client_information(&self) -> StoreFuture<'_, Option<OAuthClientInformation>> {
        let client = self.shared().client.clone();
        Box::pin(async move { Ok(client) })
    }

    fn save_client_information(&self, information: OAuthClientInformation) -> StoreFuture<'_, ()> {
        self.shared().client = Some(information);
        Box::pin(async { Ok(()) })
    }

    fn tokens(&self) -> StoreFuture<'_, Option<OAuthTokens>> {
        let tokens = self.shared().token_set.clone();
        Box::pin(async move { Ok(tokens) })
    }

    fn save_tokens(&self, tokens: OAuthTokens) -> StoreFuture<'_, ()> {
        self.shared().token_set = Some(tokens);
        Box::pin(async { Ok(()) })
    }

    fn redirect_to_authorization(&self, url: &Url) -> StoreFuture<'_, ()> {
        self.shared().authorization_url = Some(url.clone());
        Box::pin(async { Ok(()) })
    }

    fn save_code_verifier(&self, verifier: String) -> StoreFuture<'_, ()> {
        self.shared().verifier = Some(verifier);
        Box::pin(async { Ok(()) })
    }

    fn code_verifier(&self) -> StoreFuture<'_, String> {
        let verifier = self.shared().verifier.clone();
        Box::pin(async move {
            verifier.filter(|verifier| !verifier.is_empty()).ok_or(
                pi_rs_mcp::oauth::McpStoreError::Other("Missing code verifier".to_string()),
            )
        })
    }

    fn invalidate_credentials(
        &self,
        kind: pi_rs_mcp::oauth::InvalidateKind,
    ) -> StoreFuture<'_, ()> {
        let mut shared = self.shared();
        use pi_rs_mcp::oauth::InvalidateKind;
        match kind {
            InvalidateKind::All | InvalidateKind::Client => shared.client = None,
            _ => {}
        }
        match kind {
            InvalidateKind::All | InvalidateKind::Tokens => shared.token_set = None,
            _ => {}
        }
        if matches!(kind, InvalidateKind::All | InvalidateKind::Verifier) {
            shared.verifier = None;
        }
        if matches!(kind, InvalidateKind::All | InvalidateKind::Discovery) {
            shared.discovery = None;
        }
        drop(shared);
        Box::pin(async { Ok(()) })
    }

    fn save_discovery_state(&self, state: OAuthDiscoveryState) -> StoreFuture<'_, ()> {
        self.shared().discovery = Some(state);
        Box::pin(async { Ok(()) })
    }

    fn discovery_state(&self) -> StoreFuture<'_, Option<OAuthDiscoveryState>> {
        let discovery = self.shared().discovery.clone();
        Box::pin(async move { Ok(discovery) })
    }
}

#[tokio::test]
async fn discovers_registers_authorizes_with_pkce_and_refreshes_on_401() {
    let _turn = suite_turn().await;
    let refreshes = Arc::new(AtomicUsize::new(0));
    let refreshes_cell = refreshes.clone();
    let expected_challenge: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let origin = Arc::new(Mutex::new(String::new()));
    let origin_cell = origin.clone();
    let http = support::TestServer::listen(Arc::new(move |request| {
		let refreshes = refreshes_cell.clone();
		let expected_challenge = expected_challenge.clone();
		let origin_holder = origin_cell.clone();
		Box::pin(async move {
			let origin = origin_holder.lock().unwrap().clone();
			let path = request.path.clone();
			if path == "/.well-known/oauth-protected-resource/mcp" {
				return TestResponse::json(
					200,
					json!({
						"resource": format!("{origin}/mcp"),
						"authorization_servers": [origin],
						"scopes_supported": ["org:read"],
					}),
				);
			}
			if path == "/.well-known/oauth-authorization-server" {
				return TestResponse::json(
					200,
					json!({
						"issuer": origin,
						"authorization_endpoint": format!("{origin}/authorize"),
						"token_endpoint": format!("{origin}/token"),
						"registration_endpoint": format!("{origin}/register"),
						"response_types_supported": ["code"],
						"grant_types_supported": ["authorization_code", "refresh_token"],
						"token_endpoint_auth_methods_supported": ["none"],
						"code_challenge_methods_supported": ["S256"],
					}),
				);
			}
			if path == "/register" {
				let metadata = request.message.clone().unwrap_or(Value::Null);
				// Empty and null optional fields count as absent (#10266).
				return TestResponse::json(
					201,
					support::merge_into(metadata, json!({ "client_id": "test-client", "client_secret": "" })),
				);
			}
			if path == "/authorize" {
				let challenge = request
					.query
					.as_deref()
					.and_then(|query| {
						url::form_urlencoded::parse(query.as_bytes())
							.find(|(key, _)| key == "code_challenge")
							.map(|(_, value)| value.to_string())
					});
				*expected_challenge.lock().unwrap() = challenge;
				let redirect = request
					.query
					.as_deref()
					.and_then(|query| {
						url::form_urlencoded::parse(query.as_bytes())
							.find(|(key, _)| key == "redirect_uri")
							.map(|(_, value)| value.to_string())
					})
					.unwrap_or_default();
				let state = request
					.query
					.as_deref()
					.and_then(|query| {
						url::form_urlencoded::parse(query.as_bytes())
							.find(|(key, _)| key == "state")
							.map(|(_, value)| value.to_string())
					})
					.unwrap_or_default();
				let mut location = Url::parse(&redirect).unwrap_or_else(|_| Url::parse("http://127.0.0.1/").expect("stand-in URL"));
				{
					let mut pairs = location.query_pairs_mut();
					pairs.clear();
					pairs.append_pair("code", "test-code");
					pairs.append_pair("state", &state);
				}
				return TestResponse {
					status: 302,
					headers: vec![("location".into(), location.to_string())],
					body: TestBody::Empty,
				};
			}
			if path == "/token" {
				let params = support::form_params_of(&request.body);
				if form_get(&params, "grant_type") == "refresh_token" {
					refreshes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
					return TestResponse::json(
						200,
						json!({
							"access_token": "refreshed-token",
							"token_type": "Bearer",
							"refresh_token": "",
							"expires_in": null,
						}),
					);
				}
				let challenge = {
					use base64::Engine;
					use sha2::{Digest, Sha256};
					let verifier = form_get(&params, "code_verifier");
					let digest = Sha256::digest(verifier.as_bytes());
					base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
				};
				if form_get(&params, "code") != "test-code"
					|| Some(challenge.as_str()) != expected_challenge.lock().unwrap().as_deref()
				{
					return TestResponse::json(400, json!({ "error": "invalid_grant" }));
				}
				return TestResponse::json(
					200,
					json!({
						"access_token": "first-token",
						"refresh_token": "refresh-token",
						"token_type": "Bearer",
						"scope": "",
					}),
				);
			}
			if path != "/mcp" {
				return TestResponse { status: 404, headers: Vec::new(), body: TestBody::Empty };
			}
			if request.method == "GET" {
				return TestResponse { status: 405, headers: Vec::new(), body: TestBody::Empty };
			}
			if request.method == "DELETE" {
				return TestResponse { status: 200, headers: Vec::new(), body: TestBody::Empty };
			}
			let authorization = request
				.headers
				.get("authorization")
				.and_then(|value| value.to_str().ok())
				.map(str::to_string);
			if authorization.as_deref() != Some("Bearer first-token")
				&& authorization.as_deref() != Some("Bearer refreshed-token")
			{
				return TestResponse {
					status: 401,
					headers: vec![(
						"www-authenticate".into(),
						format!(
							"Bearer resource_metadata=\"{origin}/.well-known/oauth-protected-resource/mcp\", scope=\"\""
						),
					)],
					body: TestBody::Full("Unauthorized".to_string()),
				};
			}
			let message = request.message.clone().unwrap_or(Value::Null);
			if message.get("id").is_none() {
				return TestResponse { status: 202, headers: Vec::new(), body: TestBody::Empty };
			}
			let result = match message.get("method").and_then(Value::as_str) {
				Some("initialize") => json!({
					"protocolVersion": "2025-06-18",
					"capabilities": { "tools": {} },
					"serverInfo": { "name": "oauth-test", "version": "1.0.0" },
				}),
				_ => json!({ "tools": [{ "name": "issues", "inputSchema": { "type": "object" } }] }),
			};
			TestResponse::json(
				200,
				json!({ "jsonrpc": "2.0", "id": message.get("id").cloned().unwrap_or(Value::Null), "result": result }),
			)
		})
	}))
	.await;
    *origin.lock().unwrap() = http.origin.clone();
    let url = format!("{}/mcp", http.origin);

    let callback = pi_rs_mcp::oauth::OAuthCallbackServer::listen(Default::default())
        .await
        .expect("callback server listens");
    let provider = TestOAuthProvider::new(&callback.redirect_url);
    let auth = adapt_oauth_provider(provider.clone());

    let first_client = pi_rs_mcp::McpClient::new(pi_rs_mcp::McpClientOptions {
        name: "oauth-test".to_string(),
        version: "1.0.0".to_string(),
        ..Default::default()
    });
    let first_transport =
        pi_rs_mcp::StreamableHttpTransport::new(pi_rs_mcp::StreamableHttpTransportOptions {
            url: url.clone(),
            headers: Some(
                [(
                    "Authorization".to_string(),
                    "Bearer caller-supplied-stale-token".to_string(),
                )]
                .into_iter()
                .collect(),
            ),
            auth_provider: Some(auth.clone()),
            open_get_stream: Some(false),
            ..Default::default()
        })
        .expect("transport");
    let error = first_client
        .connect(first_transport)
        .await
        .expect_err("authorization needed");
    assert_eq!(
        error.to_string(),
        "MCP OAuth authorization requires user interaction"
    );
    let authorization_url = provider
        .shared()
        .authorization_url
        .clone()
        .expect("redirect prepared");
    assert_eq!(
        authorization_url
            .query_pairs()
            .find(|(key, _)| key == "scope")
            .map(|(_, value)| value.to_string()),
        Some("org:read".to_string())
    );
    assert_eq!(
        authorization_url
            .query_pairs()
            .find(|(key, _)| key == "resource")
            .map(|(_, value)| value.to_string()),
        Some(url.to_string())
    );

    let callback_server = callback.clone();
    let callback_result = {
        let callback = callback_server.clone();
        let state = provider
            .state()
            .await
            .expect("state")
            .expect("the provider returns a state");
        tokio::spawn(async move { callback.wait_for_callback(&state, None).await })
    };
    let browser = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default();
    let authorize = browser
        .get(authorization_url.as_str())
        .send()
        .await
        .expect("authorize");

    let location = authorize
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .unwrap_or_default();
    reqwest::Client::new()
        .get(&location)
        .send()
        .await
        .expect("redirect to the callback");
    let callback = callback_result
        .await
        .expect("join callback")
        .expect("the callback arrived");

    assert_eq!(
        authorize_mcp(
            provider.as_ref(),
            &pi_rs_mcp::oauth::OAuthFlowOptions {
                server_url: url.clone(),
                authorization_code: Some(callback.code.clone()),
                fetch: Some(pi_rs_mcp::McpFetch::platform()),
                ..Default::default()
            },
        )
        .await
        .expect("authorizeMcp"),
        pi_rs_mcp::oauth::OAuthFlowResult::Authorized
    );

    let client = pi_rs_mcp::McpClient::new(pi_rs_mcp::McpClientOptions {
        name: "oauth-test".to_string(),
        version: "1.0.0".to_string(),
        ..Default::default()
    });
    let transport =
        pi_rs_mcp::StreamableHttpTransport::new(pi_rs_mcp::StreamableHttpTransportOptions {
            url: url.clone(),
            headers: Some(
                [(
                    "Authorization".to_string(),
                    "Bearer caller-supplied-stale-token".to_string(),
                )]
                .into_iter()
                .collect(),
            ),
            auth_provider: Some(auth.clone()),
            open_get_stream: Some(false),
            ..Default::default()
        })
        .expect("transport");
    client.connect(transport).await.expect("connect");
    let tools = client
        .list_tools(&pi_rs_mcp::McpRequestOptions::default())
        .await
        .expect("listTools");
    assert_eq!(
        tools
            .iter()
            .map(|tool| serde_json::to_value(tool).unwrap_or_default())
            .collect::<Vec<_>>(),
        vec![json!({ "name": "issues", "inputSchema": { "type": "object" } })]
    );
    client.close().await;

    // Neither token response names a scope, so the grant has the requested scope.
    let taken = provider
        .shared()
        .token_set
        .take()
        .map(|tokens| OAuthTokens {
            access_token: "stale-token".to_string(),
            ..tokens
        });
    provider.set_token_set(taken);
    let refreshed_client = pi_rs_mcp::McpClient::new(pi_rs_mcp::McpClientOptions {
        name: "oauth-test".to_string(),
        version: "1.0.0".to_string(),
        ..Default::default()
    });
    let transport =
        pi_rs_mcp::StreamableHttpTransport::new(pi_rs_mcp::StreamableHttpTransportOptions {
            url: url.clone(),
            headers: Some(
                [(
                    "Authorization".to_string(),
                    "Bearer caller-supplied-stale-token".to_string(),
                )]
                .into_iter()
                .collect(),
            ),
            auth_provider: Some(auth),
            open_get_stream: Some(false),
            ..Default::default()
        })
        .expect("transport");
    refreshed_client.connect(transport).await.expect("connect");
    refreshed_client.close().await;
    let tokens = provider.shared().token_set.clone().expect("tokens saved");
    assert_eq!(
        serde_json::to_value(&tokens).unwrap_or_default(),
        json!({
            "access_token": "refreshed-token",
            "refresh_token": "refresh-token",
            "token_type": "Bearer",
            "scope": "org:read",
        })
    );
    assert_eq!(refreshes.load(std::sync::atomic::Ordering::SeqCst), 1);
    callback_server_close(callback_server).await;
    close_servers();
}

#[tokio::test]
async fn shares_one_refresh_between_concurrent_401s_when_refresh_tokens_rotate() {
    let _turn = suite_turn().await;
    let grants: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let origin = Arc::new(Mutex::new(String::new()));
    let origin_cell = origin.clone();
    let grants_cell = grants.clone();
    let http = support::TestServer::listen(Arc::new(move |request| {
		let grants = grants_cell.clone();
		let origin_holder = origin_cell.clone();
		Box::pin(async move {
			let origin = origin_holder.lock().unwrap().clone();
			let path = request.path.clone();
			if path == "/.well-known/oauth-protected-resource/mcp" {
				// Invalid resource metadata falls back to the server origin instead of failing
				// discovery.
				return TestResponse::json(
					200,
					json!({ "resource": format!("{origin}/mcp"), "authorization_servers": ["not a url"] }),
				);
			}
			if path == "/.well-known/oauth-authorization-server" {
				return TestResponse::json(
					200,
					json!({
						// Issuer without the trailing slash that URL parsing adds to the fallback.
						"issuer": origin,
						"authorization_endpoint": format!("{origin}/authorize"),
						"token_endpoint": format!("{origin}/token"),
						"response_types_supported": ["code"],
					}),
				);
			}
			if path == "/token" {
				let params = support::form_params_of(&request.body);
				let refresh_token = form_get(&params, "refresh_token");
				grants.lock().unwrap().push(refresh_token.clone());
				if refresh_token != "r1" {
					return TestResponse::json(400, json!({ "error": "invalid_grant" }));
				}
				tokio::time::sleep(std::time::Duration::from_millis(20)).await;
				return TestResponse::json(
					200,
					json!({ "access_token": "a2", "refresh_token": "r2", "token_type": "Bearer", "expires_in": 3600 }),
				);
			}
			TestResponse { status: 404, headers: Vec::new(), body: TestBody::Empty }
		})
	}))
	.await;
    *origin.lock().unwrap() = http.origin.clone();

    let store = MemoryOAuthStateStore::new();
    let provider =
        pi_rs_mcp::oauth::McpOAuthProvider::new(pi_rs_mcp::oauth::McpOAuthProviderOptions {
            server_url: format!("{}/mcp", http.origin),
            redirect_url: "http://127.0.0.1/callback".to_string(),
            client_metadata: OAuthClientMetadata {
                client_name: Some("test".to_string()),
                ..Default::default()
            },
            client_metadata_document: None,
            client_id: Some("client".to_string()),
            client_secret: None,
            store: Some(store.clone()),
            on_redirect: Arc::new(|_url| {}),
        });
    provider
        .save_tokens(OAuthTokens {
            access_token: "a1".to_string(),
            refresh_token: Some("r1".to_string()),
            token_type: "Bearer".to_string(),
            ..Default::default()
        })
        .await
        .expect("save tokens");
    let auth = adapt_oauth_provider(Arc::new(provider));
    let unauthorized = || pi_rs_mcp::UnauthorizedContext {
        status: 401,
        headers: pi_rs_mcp::header_map([("www-authenticate", "Bearer")]),
        server_url: Url::parse(&format!("{}/mcp", http.origin))
            .unwrap_or_else(|_| Url::parse("http://127.0.0.1/").expect("stand-in URL")),
        fetch: pi_rs_mcp::McpFetch::platform(),
        token: Some("a1".to_string()),
    };
    pi_rs_mcp::AuthProvider::on_unauthorized(auth.as_ref(), unauthorized())
        .await
        .expect("first refresh");
    pi_rs_mcp::AuthProvider::on_unauthorized(auth.as_ref(), unauthorized())
        .await
        .expect("second 401 joins the refresh");
    // A late 401 for a request that still carried the old token must not refresh again.
    pi_rs_mcp::AuthProvider::on_unauthorized(auth.as_ref(), unauthorized())
        .await
        .expect("late 401 does not refresh again");
    let grants = grants.lock().unwrap().clone();
    assert_eq!(grants, vec!["r1".to_string()]);
    let token = pi_rs_mcp::AuthProvider::token(auth.as_ref())
        .await
        .expect("token");
    assert_eq!(token.as_deref(), Some("a2"));
    let state = store
        .load()
        .await
        .expect("stored state")
        .expect("state saved");
    assert_eq!(
        state
            .tokens
            .as_ref()
            .and_then(|tokens| tokens.refresh_token.as_deref()),
        Some("r2")
    );
    assert!(
        state.tokens_expire_at.unwrap_or_default() > (support::now_millis_test() + 3_500_000.0),
        "the expiry was recorded"
    );
    close_servers();
}

#[tokio::test]
async fn asks_for_authorization_instead_of_refreshing_when_the_server_needs_more_scope() {
    let _turn = suite_turn().await;
    let provider = TestOAuthProvider::new("http://127.0.0.1/callback");
    provider.shared().client = Some(OAuthClientInformation {
        client_id: "client".to_string(),
        client_secret: None,
        client_id_issued_at: None,
        client_secret_expires_at: None,
        extra: Default::default(),
    });
    provider.shared().token_set = Some(OAuthTokens {
        access_token: "a1".to_string(),
        refresh_token: Some("r1".to_string()),
        token_type: "Bearer".to_string(),
        scope: Some("repo read:org".to_string()),
        ..Default::default()
    });
    let origin = Arc::new(Mutex::new(String::new()));
    let origin_cell = origin.clone();
    let http = support::TestServer::listen(Arc::new(move |request| {
        let origin_holder = origin_cell.clone();
        Box::pin(async move {
            let origin = origin_holder.lock().unwrap().clone();
            if request.path == "/.well-known/oauth-authorization-server" {
                return TestResponse::json(
                    200,
                    json!({
                        "issuer": origin,
                        "authorization_endpoint": format!("{origin}/authorize"),
                        "token_endpoint": format!("{origin}/token"),
                        "response_types_supported": ["code"],
                    }),
                );
            }
            TestResponse {
                status: if request.path == "/token" { 500 } else { 404 },
                headers: Vec::new(),
                body: TestBody::Empty,
            }
        })
    }))
    .await;
    *origin.lock().unwrap() = http.origin.clone();
    let auth = adapt_oauth_provider(provider.clone());
    let error = pi_rs_mcp::AuthProvider::on_unauthorized(
        auth.as_ref(),
        pi_rs_mcp::UnauthorizedContext {
            status: 403,
            headers: pi_rs_mcp::header_map([(
                "www-authenticate",
                "Bearer error=\"insufficient_scope\", scope=\"repo admin\"",
            )]),
            server_url: Url::parse(&format!("{}/mcp", http.origin))
                .unwrap_or_else(|_| Url::parse("http://127.0.0.1/").expect("stand-in URL")),
            fetch: pi_rs_mcp::McpFetch::platform(),
            token: Some("a1".to_string()),
        },
    )
    .await
    .expect_err("more scope needs a sign-in");
    assert_eq!(
        error.to_string(),
        "MCP OAuth authorization requires user interaction"
    );
    // The challenge may list only the missing scopes; the new grant keeps the old ones too.
    let authorization_url = provider
        .shared()
        .authorization_url
        .clone()
        .expect("redirect");
    assert_eq!(
        authorization_url
            .query_pairs()
            .find(|(key, _)| key == "scope")
            .map(|(_, value)| value.to_string()),
        Some("repo read:org admin".to_string())
    );
    // The working grant is kept until the user authorizes the new scope.
    assert_eq!(
        provider
            .shared()
            .token_set
            .as_ref()
            .map(|tokens| tokens.access_token.as_str()),
        Some("a1")
    );
    close_servers();
}

#[tokio::test]
async fn binds_persisted_credentials_to_the_exact_mcp_server_url() {
    let _turn = suite_turn().await;
    let store = MemoryOAuthStateStore::new();
    let first =
        pi_rs_mcp::oauth::McpOAuthProvider::new(pi_rs_mcp::oauth::McpOAuthProviderOptions {
            server_url: "https://one.example/mcp".to_string(),
            redirect_url: "http://127.0.0.1/callback".to_string(),
            client_metadata: OAuthClientMetadata {
                client_name: Some("test".to_string()),
                ..Default::default()
            },
            client_metadata_document: None,
            client_id: None,
            client_secret: None,
            store: Some(store.clone()),
            on_redirect: Arc::new(|_url| {}),
        });
    first
        .save_tokens(OAuthTokens {
            access_token: "secret".to_string(),
            token_type: "Bearer".to_string(),
            ..Default::default()
        })
        .await
        .expect("save tokens");
    let tokens = first
        .tokens()
        .await
        .expect("tokens")
        .expect("the tokens are there");
    assert_eq!(tokens.access_token, "secret");

    let second =
        pi_rs_mcp::oauth::McpOAuthProvider::new(pi_rs_mcp::oauth::McpOAuthProviderOptions {
            server_url: "https://two.example/mcp".to_string(),
            redirect_url: "http://127.0.0.1/callback".to_string(),
            client_metadata: OAuthClientMetadata {
                client_name: Some("test".to_string()),
                ..Default::default()
            },
            client_metadata_document: None,
            client_id: None,
            client_secret: None,
            store: Some(store),
            on_redirect: Arc::new(|_url| {}),
        });
    assert!(second.tokens().await.expect("tokens").is_none());
}

// #10493
#[tokio::test]
async fn registers_with_an_application_type_derived_from_the_redirect_uris_unless_one_is_set() {
    let _turn = suite_turn().await;
    let bodies: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let bodies_cell = bodies.clone();
    let origin = Arc::new(Mutex::new(String::new()));
    let origin_cell = origin.clone();
    let http = support::TestServer::listen(Arc::new(move |request| {
        let bodies = bodies_cell.clone();
        let origin_holder = origin_cell.clone();
        Box::pin(async move {
            let _origin = origin_holder.lock().unwrap().clone();
            let metadata = request.message.clone().unwrap_or(Value::Null);
            bodies.lock().unwrap().push(metadata.clone());
            TestResponse::json(
                201,
                support::merge_into(metadata, json!({ "client_id": "client" })),
            )
        })
    }))
    .await;
    *origin.lock().unwrap() = http.origin.clone();

    let register = |redirect_uris: Vec<String>, application_type: Option<String>| {
        let origin = http.origin.clone();
        async move {
            register_client(
                &origin,
                pi_rs_mcp::oauth::RegisterClientOptions {
                    metadata: None,
                    client_metadata: &OAuthClientMetadata {
                        redirect_uris: Some(redirect_uris),
                        application_type,
                        ..Default::default()
                    },
                    scope: None,
                    fetch: None,
                    signal: None,
                },
            )
            .await
            .expect("register")
        }
    };
    register(vec!["http://127.0.0.1:1234/callback".to_string()], None).await;
    register(vec!["http://[::1]/callback".to_string()], None).await;
    register(vec!["com.example.app:/callback".to_string()], None).await;
    register(vec!["https://app.example/callback".to_string()], None).await;
    register(
        vec!["http://localhost/callback".to_string()],
        Some("web".to_string()),
    )
    .await;
    let bodies = bodies.lock().unwrap().clone();
    let kinds: Vec<&str> = bodies
        .iter()
        .map(|body| {
            body.get("application_type")
                .and_then(Value::as_str)
                .unwrap_or_default()
        })
        .collect();
    assert_eq!(kinds, vec!["native", "native", "native", "web", "web"]);
    close_servers();
}

#[tokio::test]
async fn rejects_authorization_metadata_whose_issuer_does_not_match_discovery() {
    let _turn = suite_turn().await;
    let origin = Arc::new(Mutex::new(String::new()));
    let origin_cell = origin.clone();
    let http = support::TestServer::listen(Arc::new(move |request| {
        let origin_holder = origin_cell.clone();
        Box::pin(async move {
            let origin = origin_holder.lock().unwrap().clone();
            if request.path == "/.well-known/oauth-authorization-server" {
                return TestResponse::json(
                    200,
                    json!({
                        "issuer": "https://attacker.example",
                        "authorization_endpoint": format!("{origin}/authorize"),
                        "token_endpoint": format!("{origin}/token"),
                        "response_types_supported": ["code"],
                    }),
                );
            }
            TestResponse {
                status: 404,
                headers: Vec::new(),
                body: TestBody::Empty,
            }
        })
    }))
    .await;
    *origin.lock().unwrap() = http.origin.clone();
    let error = discover_authorization_server_metadata(&http.origin, Default::default())
        .await
        .expect_err("issuer mismatch");
    assert!(matches!(
        error,
        OAuthError::IssuerMismatch {
            expected: Some(_),
            received: Some(_)
        }
    ));
    close_servers();
}

#[tokio::test]
async fn rejects_non_http_authorization_endpoints_which_the_platform_url_handler_would_launch() {
    let _turn = suite_turn().await;
    let endpoints = [
        "file://attacker.example/share/x.exe",
        "search-ms:query=x",
        "ms-msdt:/id",
    ];
    let origin = Arc::new(Mutex::new(String::new()));
    let origin_cell = origin.clone();
    let endpoint_cell = Arc::new(Mutex::new(String::new()));
    let endpoint_cell_for_server = endpoint_cell.clone();
    let http = support::TestServer::listen(Arc::new(move |_request| {
        let origin_holder = origin_cell.clone();
        let endpoint_holder = endpoint_cell_for_server.clone();
        Box::pin(async move {
            let origin = origin_holder.lock().unwrap().clone();
            let endpoint = endpoint_holder.lock().unwrap().clone();
            TestResponse::json(
                200,
                json!({
                    "issuer": origin,
                    "authorization_endpoint": endpoint,
                    "token_endpoint": format!("{origin}/token"),
                    "response_types_supported": ["code"],
                }),
            )
        })
    }))
    .await;
    *origin.lock().unwrap() = http.origin.clone();
    for value in endpoints {
        *endpoint_cell.lock().unwrap() = value.to_string();
        let error = discover_authorization_server_metadata(&http.origin, Default::default())
            .await
            .expect_err("invalid endpoint");
        assert!(
            error.to_string().contains("Invalid authorization endpoint"),
            "error was {error}"
        );
        let provider = TestOAuthProvider::new("http://127.0.0.1/callback");
        provider.shared().client = Some(OAuthClientInformation {
            client_id: "client".to_string(),
            client_secret: None,
            client_id_issued_at: None,
            client_secret_expires_at: None,
            extra: Default::default(),
        });
        let metadata = json!({
            "issuer": http.origin,
            "authorization_endpoint": value,
            "token_endpoint": format!("{}/token", http.origin),
            "response_types_supported": ["code"],
        });
        let metadata_parsed: Option<AuthorizationServerMetadata> =
            serde_json::from_value(metadata).ok();
        let client = provider.shared().client.clone().expect("client");
        let error = start_authorization(
            &http.origin,
            pi_rs_mcp::oauth::StartAuthorizationOptions {
                metadata: metadata_parsed.as_ref(),
                client_information: &client,
                redirect_url: &provider.redirect_url,
                scope: None,
                state: None,
                resource: None,
            },
        )
        .await
        .expect_err("invalid endpoint in start");
        assert!(
            error.to_string().contains("Invalid authorization endpoint"),
            "error was {error}"
        );
    }
    close_servers();
}

// #10172
#[tokio::test]
async fn uses_a_configured_authorization_server_metadata_document_as_is() {
    let _turn = suite_turn().await;
    let origin = Arc::new(Mutex::new(String::new()));
    let origin_cell = origin.clone();
    let http = support::TestServer::listen(Arc::new(move |request| {
		let origin_holder = origin_cell.clone();
		Box::pin(async move {
			let origin = origin_holder.lock().unwrap().clone();
			match request.path.as_str() {
				"/.well-known/oauth-protected-resource/mcp" => {
					// Names the MCP server itself, which serves no authorization server metadata.
					TestResponse::json(
						200,
						json!({ "resource": format!("{origin}/mcp"), "authorization_servers": [origin] }),
					)
				}
				"/idp/metadata.json" => TestResponse::json(
					200,
					json!({
						// Not derivable from the document URL; a configured document is not checked.
						"issuer": "https://idp.example",
						"authorization_endpoint": format!("{origin}/idp/authorize"),
						"token_endpoint": format!("{origin}/idp/token"),
						"response_types_supported": ["code"],
					}),
				),
				_ => TestResponse { status: 404, headers: Vec::new(), body: TestBody::Empty },
			}
		})
	}))
	.await;
    *origin.lock().unwrap() = http.origin.clone();
    let provider = TestOAuthProvider::new("http://127.0.0.1/callback");
    provider.shared().client = Some(OAuthClientInformation {
        client_id: "client".to_string(),
        client_secret: None,
        client_id_issued_at: None,
        client_secret_expires_at: None,
        extra: Default::default(),
    });
    let options = pi_rs_mcp::oauth::OAuthFlowOptions {
        server_url: format!("{}/mcp", http.origin),
        authorization_server_metadata_url: Some(
            Url::parse(&format!("{}/idp/metadata.json", http.origin))
                .expect("the document URL parses"),
        ),
        ..Default::default()
    };
    assert_eq!(
        authorize_mcp(provider.as_ref(), &options)
            .await
            .expect("authorizeMcp"),
        pi_rs_mcp::oauth::OAuthFlowResult::Redirect
    );
    let authorization_url = provider
        .shared()
        .authorization_url
        .clone()
        .expect("redirect");
    assert_eq!(
        format!(
            "{}{}",
            authorization_url.origin().ascii_serialization(),
            authorization_url.path()
        ),
        format!("{}/idp/authorize", http.origin)
    );
    assert_eq!(
        authorization_url
            .query_pairs()
            .find(|(key, _)| key == "resource")
            .map(|(_, value)| value.to_string()),
        Some(format!("{}/mcp", http.origin))
    );

    let insecure = pi_rs_mcp::oauth::OAuthFlowOptions {
        authorization_server_metadata_url: Some(
            Url::parse("http://idp.example/metadata.json").expect("the insecure URL parses"),
        ),
        ..options.clone()
    };
    let error = authorize_mcp(provider.as_ref(), &insecure)
        .await
        .expect_err("insecure document");
    assert!(
        matches!(error, OAuthError::Insecure { .. }),
        "error was {error:?}"
    );
    close_servers();
}

#[tokio::test]
async fn exchanges_a_code_only_when_its_iss_parameter_names_the_authorization_server() {
    let _turn = suite_turn().await;
    let codes: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let origin = Arc::new(Mutex::new(String::new()));
    let origin_cell = origin.clone();
    let codes_cell = codes.clone();
    let http = support::TestServer::listen(Arc::new(move |request| {
        let codes = codes_cell.clone();
        let origin_holder = origin_cell.clone();
        Box::pin(async move {
            let _origin = origin_holder.lock().unwrap().clone();
            if request.method != "POST" {
                return TestResponse {
                    status: 404,
                    headers: Vec::new(),
                    body: TestBody::Empty,
                };
            }
            let params = support::form_params_of(&request.body);
            codes.lock().unwrap().push(form_get(&params, "code"));
            TestResponse::json(
                200,
                json!({ "access_token": "token", "token_type": "Bearer" }),
            )
        })
    }))
    .await;
    *origin.lock().unwrap() = http.origin.clone();

    let http_origin = http.origin.clone();
    let exchange =
        move |code: String, iss: Option<String>, iss_parameter_supported: bool, metadata: bool| {
            let http_origin = http_origin.clone();
            let provider = TestOAuthProvider::new("http://127.0.0.1/callback");
            provider.shared().client = Some(OAuthClientInformation {
                client_id: "client".to_string(),
                client_secret: None,
                client_id_issued_at: None,
                client_secret_expires_at: None,
                extra: Default::default(),
            });
            provider.shared().verifier = Some("verifier".to_string());
            let metadata_json = json!({
                "issuer": http_origin,
                "authorization_endpoint": format!("{http_origin}/authorize"),
                "token_endpoint": format!("{http_origin}/token"),
                "response_types_supported": ["code"],
                "authorization_response_iss_parameter_supported": iss_parameter_supported,
            });
            provider.shared().discovery = Some(OAuthDiscoveryState {
                authorization_server_url: http_origin.clone(),
                authorization_server_metadata: metadata.then(|| {
                    serde_json::from_value::<AuthorizationServerMetadata>(metadata_json)
                        .expect("metadata parses")
                }),
                resource_metadata: None,
                resource_metadata_url: None,
            });
            let server_url = format!("{http_origin}/mcp");
            async move {
                authorize_mcp(
                    provider.as_ref(),
                    &pi_rs_mcp::oauth::OAuthFlowOptions {
                        server_url,
                        authorization_code: Some(code),
                        iss,
                        ..Default::default()
                    },
                )
                .await
            }
        };

    assert!(matches!(
        exchange(
            "other".to_string(),
            Some("https://attacker.example".to_string()),
            false,
            true
        )
        .await
        .expect_err("a foreign iss"),
        OAuthError::IssuerMismatch { .. }
    ));
    assert!(matches!(
        exchange("missing".to_string(), None, true, true)
            .await
            .expect_err("a missing iss"),
        OAuthError::IssuerMismatch { .. }
    ));
    assert_eq!(
        exchange(
            "matching".to_string(),
            Some(http.origin.clone()),
            true,
            true
        )
        .await
        .expect("a matching iss"),
        pi_rs_mcp::oauth::OAuthFlowResult::Authorized
    );
    // Servers that do not promise the parameter may omit it.
    assert_eq!(
        exchange("omitted".to_string(), None, false, true)
            .await
            .expect("an omitted iss"),
        pi_rs_mcp::oauth::OAuthFlowResult::Authorized
    );
    // Without metadata there is no issuer to compare with, so a server cannot skip the check by
    // hiding it.
    assert!(matches!(
        exchange(
            "unverifiable".to_string(),
            Some("https://attacker.example".to_string()),
            false,
            false
        )
        .await
        .expect_err("no metadata to verify against"),
        OAuthError::IssuerMismatch { .. }
    ));
    assert_eq!(
        exchange("no-metadata".to_string(), None, false, false)
            .await
            .expect("no metadata, no iss"),
        pi_rs_mcp::oauth::OAuthFlowResult::Authorized
    );
    let codes = codes.lock().unwrap().clone();
    assert_eq!(codes, vec!["matching", "omitted", "no-metadata"]);
    close_servers();
}

// #10565
#[tokio::test]
async fn stops_when_its_signal_aborts_without_falling_back_to_a_redirect() {
    let _turn = suite_turn().await;
    let stalled: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let origin = Arc::new(Mutex::new(String::new()));
    let _origin_cell = origin.clone();
    let stalled_cell = stalled.clone();
    let http = support::TestServer::listen(Arc::new(move |request| {
        let stalled = stalled_cell.clone();
        Box::pin(async move {
            // Accepts every request and never answers.
            stalled.lock().unwrap().push(request.path.clone());
            TestResponse {
                status: 200,
                headers: Vec::new(),
                body: TestBody::Hang,
            }
        })
    }))
    .await;
    *origin.lock().unwrap() = http.origin.clone();

    // Discovery stalls; so does a refresh that would otherwise fall back to a redirect.
    let discovery_provider = TestOAuthProvider::new("http://127.0.0.1/callback");
    let refreshing = TestOAuthProvider::new("http://127.0.0.1/callback");
    refreshing.shared().client = Some(OAuthClientInformation {
        client_id: "client".to_string(),
        client_secret: None,
        client_id_issued_at: None,
        client_secret_expires_at: None,
        extra: Default::default(),
    });
    refreshing.shared().token_set = Some(OAuthTokens {
        access_token: "a1".to_string(),
        refresh_token: Some("r1".to_string()),
        token_type: "Bearer".to_string(),
        ..Default::default()
    });
    refreshing.shared().discovery = Some(OAuthDiscoveryState {
        authorization_server_url: http.origin.clone(),
        authorization_server_metadata: Some(AuthorizationServerMetadata {
            issuer: http.origin.clone(),
            authorization_endpoint: format!("{}/authorize", http.origin),
            token_endpoint: format!("{}/token", http.origin),
            response_types_supported: vec!["code".to_string()],
            ..Default::default()
        }),
        resource_metadata: None,
        resource_metadata_url: None,
    });

    for (provider, expected_path) in [
        (
            discovery_provider,
            "/.well-known/oauth-protected-resource/mcp",
        ),
        (refreshing, "/token"),
    ] {
        let before = stalled.lock().unwrap().len();
        let controller = pi_rs_mcp::AbortController::new();
        let signal = controller.signal();
        let provider_for_flow = provider.clone();
        let flow_options = pi_rs_mcp::oauth::OAuthFlowOptions {
            server_url: format!("{}/mcp", http.origin),
            signal: Some(signal),
            ..Default::default()
        };
        let flow =
            tokio::spawn(
                async move { authorize_mcp(provider_for_flow.as_ref(), &flow_options).await },
            );
        // Abort once the stalled server saw the flow's first request.
        let seen = support::wait_for(
            {
                let stalled = stalled.clone();
                move || stalled.lock().unwrap().len() > before
            },
            2_000,
        )
        .await;
        assert!(seen, "the stalled request arrived");
        controller.abort(None);
        let error = flow
            .await
            .expect("join flow")
            .expect_err("the abort stops the flow");
        assert!(
            error.to_string().contains("The operation was aborted"),
            "error was {error}"
        );
        assert!(
            provider.shared().authorization_url.is_none(),
            "no authorization URL was prepared"
        );
        let stalled = stalled.lock().unwrap().clone();
        assert_eq!(stalled.last().map(String::as_str), Some(expected_path));
    }
    close_servers();
}

#[tokio::test]
async fn renders_plain_text_by_default_on_the_callback_server() {
    let _turn = suite_turn().await;
    let callback = pi_rs_mcp::oauth::OAuthCallbackServer::listen(Default::default())
        .await
        .expect("callback server listens");
    let pending = {
        let callback = callback.clone();
        tokio::spawn(async move {
            callback
                .wait_for_callback("s1", None)
                .await
                .expect("the callback arrives")
        })
    };
    let response = reqwest::Client::new()
        .get(format!("{}?code=abc&state=s1", callback.redirect_url))
        .send()
        .await
        .expect("the page loads");
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(
        response.text().await.expect("the page body"),
        "Authorization complete. You may close this window."
    );
    let callback_response = pending.await.expect("join wait");
    assert_eq!(callback_response.code, "abc");
    callback_server_close(callback).await;
}

// #11311
#[tokio::test]
async fn closes_while_the_browser_keeps_an_unused_connection_open() {
    let _turn = suite_turn().await;
    let callback = pi_rs_mcp::oauth::OAuthCallbackServer::listen(Default::default())
        .await
        .expect("callback server listens");
    let url = Url::parse(&callback.redirect_url).expect("the redirect URL parses");
    let spare = tokio::net::TcpStream::connect((
        url.host_str().unwrap_or("127.0.0.1"),
        url.port().unwrap_or(80),
    ))
    .await
    .expect("the spare connection opens");
    let pending = {
        let callback = callback.clone();
        tokio::spawn(async move {
            callback
                .wait_for_callback("s1", None)
                .await
                .expect("the callback arrives")
        })
    };
    let response = reqwest::Client::new()
        .get(format!("{}?code=abc&state=s1", callback.redirect_url))
        .send()
        .await
        .expect("the page loads");
    assert_eq!(
        response.text().await.expect("the page body"),
        "Authorization complete. You may close this window."
    );
    let callback_response = pending.await.expect("join wait");
    assert_eq!(callback_response.code, "abc");
    callback.close().await;
    drop(spare);
}

// #10302
#[tokio::test]
async fn rejects_a_response_on_another_path_than_the_expected_one() {
    let _turn = suite_turn().await;
    let callback = pi_rs_mcp::oauth::OAuthCallbackServer::listen(
        pi_rs_mcp::oauth::OAuthCallbackServerOptions {
            extra_paths: vec!["/callback/server-id".to_string()],
            ..Default::default()
        },
    )
    .await
    .expect("callback server listens");
    let origin = Url::parse(&callback.redirect_url)
        .expect("the redirect URL parses")
        .origin()
        .ascii_serialization();
    {
        let callback = callback.clone();
        let mixed_up = tokio::spawn(async move {
            callback
                .wait_for_callback("s1", Some("/callback/server-id"))
                .await
        });
        let wrong = reqwest::Client::new()
            .get(format!("{origin}/callback?code=abc&state=s1"))
            .send()
            .await
            .expect("the mixed-up redirect loads");
        assert_eq!(wrong.status().as_u16(), 400);
        let error = mixed_up
            .await
            .expect("join wait")
            .expect_err("the response arrived on another path");
        assert_eq!(
            error.to_string(),
            "The authorization response arrived on another redirect URI"
        );
    }
    {
        let callback = callback.clone();
        let pending = tokio::spawn(async move {
            callback
                .wait_for_callback("s2", Some("/callback/server-id"))
                .await
                .expect("the callback arrives")
        });
        let right = reqwest::Client::new()
            .get(format!("{origin}/callback/server-id?code=abc&state=s2"))
            .send()
            .await
            .expect("the right redirect loads");
        assert_eq!(right.status().as_u16(), 200);
        let callback_response = pending.await.expect("join wait");
        assert_eq!(callback_response.code, "abc");
    }
    callback_server_close(callback).await;
}

#[tokio::test]
async fn renders_pages_through_render_page() {
    let _turn = suite_turn().await;
    let pages: Arc<Mutex<Vec<OAuthCallbackPage>>> = Arc::new(Mutex::new(Vec::new()));
    let pages_for_render = pages.clone();
    let callback = pi_rs_mcp::oauth::OAuthCallbackServer::listen(
        pi_rs_mcp::oauth::OAuthCallbackServerOptions {
            render_page: Some(Arc::new(move |page: &OAuthCallbackPage| {
                pages_for_render.lock().unwrap().push(page.clone());
                match page {
                    OAuthCallbackPage::Ok => "<p>ok</p>".to_string(),
                    OAuthCallbackPage::Failed { message, .. } => format!("<p>{message}</p>"),
                }
            })),
            ..Default::default()
        },
    )
    .await
    .expect("callback server listens");
    {
        let waiting_callback = callback.clone();
        let denied =
            tokio::spawn(async move { waiting_callback.wait_for_callback("s1", None).await });
        let failure = reqwest::Client::new()
            .get(format!(
                "{}?error=access_denied&error_description=Denied&state=s1",
                callback.redirect_url
            ))
            .send()
            .await
            .expect("the failed authorization loads");
        assert_eq!(
            failure
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("text/html; charset=utf-8")
        );
        let error = denied
            .await
            .expect("join wait")
            .expect_err("the authorization was denied");
        assert_eq!(error.to_string(), "Denied");
        let pages = pages.lock().unwrap().clone();
        assert_eq!(
            pages.last(),
            Some(&OAuthCallbackPage::Failed {
                message: "Authorization failed. You may close this window.".to_string(),
                details: Some("Denied".to_string()),
            })
        );
    }
    {
        let waiting_callback = callback.clone();
        let pending = tokio::spawn(async move {
            waiting_callback
                .wait_for_callback("s2", None)
                .await
                .expect("the callback arrives")
        });
        let success = reqwest::Client::new()
            .get(format!("{}?code=abc&state=s2", callback.redirect_url))
            .send()
            .await
            .expect("the successful authorization loads");
        assert_eq!(success.text().await.expect("the page body"), "<p>ok</p>");
        let callback_response = pending.await.expect("join wait");
        assert_eq!(callback_response.code, "abc");
    }
    callback_server_close(callback).await;
}
