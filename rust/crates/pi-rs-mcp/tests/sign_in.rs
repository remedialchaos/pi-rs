//! `packages/mcp/test/sign-in.test.ts`.

use std::sync::Arc;

use serde_json::{Value, json};

use pi_rs_mcp::oauth::{
    ClientRegistration, McpClientMetadataDocuments, McpOAuthSettings, McpSignInOptions,
    McpSignInRedirect, McpSignInStart, OAuthCallback, OAuthChallenge, OAuthError,
    complete_mcp_sign_in, start_mcp_sign_in,
};

#[path = "support/mod.rs"]
#[expect(
    dead_code,
    reason = "shared test helpers: each integration-test binary uses a different subset"
)]
mod support;

use support::{
    AuthorizationServer, AuthorizationServerOptions, OAuthCallbackResponse, close_servers,
    form_get, memory_store, recording_store, start_authorization_server, suite_turn,
    wait_lock_runs,
};

/// Callback routes of the application on two origins, like p2 through a relay and on its public URL.
const ROUTE: &str = "https://app.example/mcp/callback";
const OTHER_ROUTE: &str = "https://relay.example/mcp/callback";
/// A loopback redirect URI; tests read the redirect from the authorization server instead of following it.
const LOOPBACK: &str = "http://127.0.0.1:1/callback";

/// Documents that list `/callback` on 127.0.0.1, like pi's on pi.dev.
struct LoopbackDocuments;

impl McpClientMetadataDocuments for LoopbackDocuments {
    fn base_url(&self) -> String {
        "https://app.example/oauth".to_string()
    }

    fn lists_redirect_url(&self, redirect_url: &str) -> bool {
        match url::Url::parse(redirect_url) {
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

fn documents() -> Arc<dyn McpClientMetadataDocuments> {
    Arc::new(LoopbackDocuments)
}

fn sign_in_options(server: &Arc<AuthorizationServer>, redirect_url: &str) -> McpSignInOptions {
    McpSignInOptions {
        server_url: server.server_url.clone(),
        store: memory_store(),
        settings: McpOAuthSettings::default(),
        client_name: "app".to_string(),
        client_metadata_documents: None,
        redirect_url: redirect_url.to_string(),
        challenge: None,
        signal: None,
    }
}

async fn start_redirect(options: &McpSignInOptions) -> McpSignInRedirect {
    match start_mcp_sign_in(options.clone())
        .await
        .expect("startMcpSignIn")
    {
        McpSignInStart::Authorized => panic!("Expected the browser step"),
        McpSignInStart::Redirect(redirect) => redirect,
    }
}

/// The response a callback route reads from the query of the redirect.
fn route_response(location: &str) -> OAuthCallback {
    OAuthCallbackResponse::from_url(location).into_callback()
}

/// Start, follow the browser step, and complete.
async fn sign_in(options: &McpSignInOptions) -> McpSignInRedirect {
    let authorization = start_redirect(options).await;
    let location = support::authorize_in_browser(&authorization.authorization_url).await;
    complete_mcp_sign_in(
        options.clone(),
        authorization.clone(),
        route_response(&location),
    )
    .await
    .expect("completeMcpSignIn");
    authorization
}

#[tokio::test]
async fn completes_in_a_second_call_with_the_same_store_and_a_persisted_redirect() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions::default()).await;
    let options = McpSignInOptions {
        settings: McpOAuthSettings {
            scope: Some("issues:read".to_string()),
            ..Default::default()
        },
        ..sign_in_options(&server, ROUTE)
    };

    let started = start_redirect(&options).await;
    // Plain data: the application may store it and read it back in another request.
    let stored = serde_json::to_value(&started).expect("serialize");
    let authorization: McpSignInRedirect =
        serde_json::from_value(stored).expect("the redirect is plain data");
    assert_eq!(authorization, started);
    assert_eq!(authorization.redirect_url, ROUTE);
    assert_eq!(authorization.scope.as_deref(), Some("issues:read"));
    assert_eq!(
        url::Url::parse(&authorization.authorization_url)
            .ok()
            .and_then(|url| {
                url.query_pairs()
                    .find(|(key, _)| key == "state")
                    .map(|(_, value)| value.to_string())
            })
            .as_deref(),
        Some(authorization.state.as_str())
    );
    let registrations = server.registrations.lock().unwrap().clone();
    assert_eq!(registrations.len(), 1);
    assert_eq!(
        registrations[0]
            .get("client_name")
            .and_then(|name| name.as_str()),
        Some("app")
    );
    let registered_redirect = registrations[0]
        .get("redirect_uris")
        .and_then(|uris| uris.as_array().cloned())
        .unwrap_or_default();
    assert_eq!(registered_redirect, vec![json!(ROUTE)]);
    assert_eq!(
        registrations[0]
            .get("application_type")
            .and_then(|kind| kind.as_str()),
        Some("web")
    );

    let location = support::authorize_in_browser(&authorization.authorization_url).await;
    complete_mcp_sign_in(options.clone(), authorization, route_response(&location))
        .await
        .expect("completeMcpSignIn");

    let state = options
        .store
        .load()
        .await
        .expect("stored state")
        .expect("the sign-in stored its state");
    let tokens = state.tokens.expect("tokens were saved");
    assert_eq!(tokens.access_token, "access-1");
    assert_eq!(tokens.scope.as_deref(), Some("issues:read"));
    let grants = server.token_requests.lock().unwrap().clone();
    assert_eq!(form_get(&grants[0], "redirect_uri"), ROUTE);
    assert_eq!(form_get(&grants[0], "client_id"), "client-1");
    close_servers();
}

#[tokio::test]
async fn refreshes_the_stored_tokens_instead_of_redirecting() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions::default()).await;
    let options = sign_in_options(&server, ROUTE);
    sign_in(&options).await;

    assert_eq!(
        start_mcp_sign_in(options.clone())
            .await
            .expect("startMcpSignIn"),
        McpSignInStart::Authorized
    );
    assert_eq!(server.authorizations.lock().unwrap().len(), 1);
    let grants = server.token_requests.lock().unwrap().clone();
    assert_eq!(form_get(&grants[1], "grant_type"), "refresh_token");
    let state = options
        .store
        .load()
        .await
        .expect("stored state")
        .expect("the sign-in stored its state");
    assert_eq!(
        state.tokens.map(|tokens| tokens.access_token).as_deref(),
        Some("access-2")
    );
    close_servers();
}

#[tokio::test]
async fn refreshes_under_the_stores_refresh_lock() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions::default()).await;
    let recording = recording_store(None);
    let options = McpSignInOptions {
        store: recording.as_server_store(),
        ..sign_in_options(&server, ROUTE)
    };
    sign_in(&options).await;

    assert_eq!(
        start_mcp_sign_in(options.clone())
            .await
            .expect("startMcpSignIn"),
        McpSignInStart::Authorized
    );
    let saves = recording.saves.lock().unwrap().clone();
    assert_eq!(
        saves
            .iter()
            .filter(|save| save.access_token.as_deref() == Some("access-2"))
            .cloned()
            .collect::<Vec<_>>(),
        vec![support::RecordedSave {
            locked: true,
            access_token: Some("access-2".to_string())
        }],
    );
    close_servers();
}

#[tokio::test]
async fn goes_to_the_browser_step_when_another_process_holds_the_refresh_lock_for_too_long() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions::default()).await;
    let options = McpSignInOptions {
        store: recording_store(Some("refresh lock busy".to_string())).as_server_store(),
        ..sign_in_options(&server, ROUTE)
    };
    sign_in(&options).await;

    assert!(matches!(
        start_mcp_sign_in(options.clone())
            .await
            .expect("startMcpSignIn"),
        McpSignInStart::Redirect(_)
    ));
    let grants = server.token_requests.lock().unwrap().clone();
    assert!(
        grants
            .iter()
            .all(|grant| form_get(grant, "grant_type") == "authorization_code")
    );
    close_servers();
}

#[tokio::test]
async fn fails_when_the_refresh_lock_fails_for_another_reason() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions::default()).await;
    let lock_error = "EACCES: permission denied".to_string();
    let options = McpSignInOptions {
        store: recording_store(Some(lock_error.clone())).as_server_store(),
        ..sign_in_options(&server, ROUTE)
    };

    let error = start_mcp_sign_in(options)
        .await
        .expect_err("the lock error fails the sign-in");
    assert_eq!(error, OAuthError::Message(lock_error));
    assert!(server.registrations.lock().unwrap().is_empty());
    assert!(server.authorizations.lock().unwrap().is_empty());
    close_servers();
}

#[tokio::test]
async fn refuses_to_complete_with_another_redirect_uri_than_the_authorization_request() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions {
        cimd: true,
        ..Default::default()
    })
    .await;
    let options = McpSignInOptions {
        redirect_url: LOOPBACK.to_string(),
        client_metadata_documents: Some(documents()),
        ..sign_in_options(&server, LOOPBACK)
    };
    let authorization = start_redirect(&options).await;
    let location = support::authorize_in_browser(&authorization.authorization_url).await;
    let response = route_response(&location);
    // Another loopback port than the one passed to start: the server-specific variant differs too.
    let wrong = McpSignInOptions {
        redirect_url: "http://127.0.0.1:2/callback".to_string(),
        ..options.clone()
    };
    let error = complete_mcp_sign_in(wrong, authorization.clone(), response)
        .await
        .expect_err("another redirect URI");
    assert!(
        error.to_string().contains(&format!(
            "but the authorization request used {}",
            authorization.redirect_url
        )),
        "error was {error}"
    );
    assert!(server.token_requests.lock().unwrap().is_empty());
    close_servers();
}

#[tokio::test]
async fn gives_every_sign_in_a_fresh_state_and_refuses_to_complete_a_superseded_one() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions::default()).await;
    let options = sign_in_options(&server, ROUTE);
    let first = start_redirect(&options).await;
    let first_location = support::authorize_in_browser(&first.authorization_url).await;
    let first_response = route_response(&first_location);
    let second = start_redirect(&options).await;
    assert_ne!(second.state, first.state);

    let error = complete_mcp_sign_in(options.clone(), first.clone(), first_response)
        .await
        .expect_err("the first sign-in was superseded");
    assert_eq!(
        error,
        OAuthError::Message(
            "Another sign-in to this MCP server started since; sign in again".to_string()
        )
    );
    assert!(server.token_requests.lock().unwrap().is_empty());
    close_servers();
}

#[tokio::test]
async fn refuses_a_response_with_another_state() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions::default()).await;
    let options = sign_in_options(&server, ROUTE);
    let authorization = start_redirect(&options).await;
    let location = support::authorize_in_browser(&authorization.authorization_url).await;
    let mut response = route_response(&location);
    response.state = "other".to_string();
    let error = complete_mcp_sign_in(options, authorization, response)
        .await
        .expect_err("another state");
    assert_eq!(
        error,
        OAuthError::Message(
            "The authorization response belongs to a different sign-in".to_string()
        )
    );
    assert!(server.token_requests.lock().unwrap().is_empty());
    close_servers();
}

#[tokio::test]
async fn refreshes_with_the_stored_client_from_another_redirect_uri() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions::default()).await;
    let options = sign_in_options(&server, ROUTE);
    sign_in(&options).await;

    let other = McpSignInOptions {
        redirect_url: OTHER_ROUTE.to_string(),
        ..options.clone()
    };
    assert_eq!(
        start_mcp_sign_in(other).await.expect("startMcpSignIn"),
        McpSignInStart::Authorized
    );
    assert_eq!(server.registrations.lock().unwrap().len(), 1);
    let grants = server.token_requests.lock().unwrap().clone();
    assert_eq!(form_get(&grants[1], "client_id"), "client-1");
    close_servers();
}

#[tokio::test]
async fn keeps_the_client_for_a_browser_step_on_its_redirect_uri() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions::default()).await;
    let options = sign_in_options(&server, ROUTE);
    sign_in(&options).await;
    server.revoke_refresh_tokens();

    let authorization = start_redirect(&options).await;
    assert_eq!(
        url::Url::parse(&authorization.authorization_url)
            .ok()
            .and_then(|url| {
                url.query_pairs()
                    .find(|(key, _)| key == "client_id")
                    .map(|(_, value)| value.to_string())
            })
            .as_deref(),
        Some("client-1")
    );
    assert_eq!(server.registrations.lock().unwrap().len(), 1);
    close_servers();
}

#[tokio::test]
async fn drops_the_client_and_its_tokens_for_a_browser_step_on_another_redirect_uri() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions::default()).await;
    let options = sign_in_options(&server, ROUTE);
    sign_in(&options).await;
    server.revoke_refresh_tokens();

    let other = McpSignInOptions {
        redirect_url: OTHER_ROUTE.to_string(),
        ..options.clone()
    };
    let authorization = start_redirect(&other).await;
    assert_eq!(
        url::Url::parse(&authorization.authorization_url)
            .ok()
            .and_then(|url| {
                url.query_pairs()
                    .find(|(key, _)| key == "client_id")
                    .map(|(_, value)| value.to_string())
            })
            .as_deref(),
        Some("client-2")
    );
    let registrations = server.registrations.lock().unwrap().clone();
    assert_eq!(
        registrations[1]
            .get("redirect_uris")
            .and_then(|uris| uris.as_array().cloned()),
        Some(vec![json!(OTHER_ROUTE)])
    );
    let state = other
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
        Some("client-2")
    );
    assert!(state.tokens.is_none());
    close_servers();
}

#[tokio::test]
async fn does_not_refresh_with_a_registered_client_that_cimd_replaces() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions {
        cimd: true,
        iss_parameter: true,
    })
    .await;
    let options = McpSignInOptions {
        redirect_url: LOOPBACK.to_string(),
        client_metadata_documents: Some(documents()),
        ..sign_in_options(&server, LOOPBACK)
    };
    let with_dcr = McpSignInOptions {
        settings: McpOAuthSettings {
            client_registration: Some(ClientRegistration::Dcr),
            ..Default::default()
        },
        ..options.clone()
    };
    sign_in(&with_dcr).await;
    let state = with_dcr
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
        Some("client-1")
    );

    let with_cimd = McpSignInOptions {
        settings: McpOAuthSettings {
            client_registration: Some(ClientRegistration::Cimd),
            ..Default::default()
        },
        ..options.clone()
    };
    let authorization = start_redirect(&with_cimd).await;
    assert_eq!(
        url::Url::parse(&authorization.authorization_url)
            .ok()
            .and_then(|url| {
                url.query_pairs()
                    .find(|(key, _)| key == "client_id")
                    .map(|(_, value)| value.to_string())
            })
            .as_deref(),
        Some("https://app.example/oauth/client.json")
    );
    let grants = server.token_requests.lock().unwrap().clone();
    assert!(
        grants
            .iter()
            .all(|grant| form_get(grant, "grant_type") == "authorization_code")
    );
    let state = with_cimd
        .store
        .load()
        .await
        .expect("stored state")
        .expect("the sign-in stored its state");
    assert!(state.client_information.is_none());
    assert!(state.tokens.is_none());
    close_servers();
}

#[tokio::test]
async fn asks_for_the_granted_scope_plus_the_challenged_one_on_a_step_up_without_refreshing() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions::default()).await;
    let options = McpSignInOptions {
        challenge: Some(OAuthChallenge {
            scope: Some("issues:read".to_string()),
            ..Default::default()
        }),
        ..sign_in_options(&server, ROUTE)
    };
    sign_in(&options).await;
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
            .and_then(|tokens| tokens.scope.as_deref()),
        Some("issues:read")
    );

    // The challenge lists only the missing scope; requesting just that would lose issues:read.
    let step_up = McpSignInOptions {
        challenge: Some(OAuthChallenge {
            error: Some("insufficient_scope".to_string()),
            scope: Some("issues:write".to_string()),
            ..Default::default()
        }),
        ..options.clone()
    };
    let authorization = sign_in(&step_up).await;
    assert_eq!(
        authorization.scope.as_deref(),
        Some("issues:read issues:write")
    );
    let grants = server.token_requests.lock().unwrap().clone();
    assert!(
        grants
            .iter()
            .all(|grant| form_get(grant, "grant_type") == "authorization_code")
    );
    let state = step_up
        .store
        .load()
        .await
        .expect("stored state")
        .expect("the sign-in stored its state");
    assert_eq!(
        state
            .tokens
            .as_ref()
            .and_then(|tokens| tokens.scope.as_deref()),
        Some("issues:read issues:write")
    );
    close_servers();
}

/// The loopback-flavored options of the client-metadata-document tests.
fn loopback_options(server: &Arc<AuthorizationServer>) -> McpSignInOptions {
    McpSignInOptions {
        redirect_url: LOOPBACK.to_string(),
        client_metadata_documents: Some(documents()),
        ..sign_in_options(server, LOOPBACK)
    }
}

#[tokio::test]
async fn uses_a_document_when_the_server_supports_them_and_the_documents_list_the_redirect_uri() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions {
        cimd: true,
        iss_parameter: true,
    })
    .await;
    let options = loopback_options(&server);
    let authorization = sign_in(&options).await;
    assert_eq!(
        url::Url::parse(&authorization.authorization_url)
            .ok()
            .and_then(|url| {
                url.query_pairs()
                    .find(|(key, _)| key == "client_id")
                    .map(|(_, value)| value.to_string())
            })
            .as_deref(),
        Some("https://app.example/oauth/client.json")
    );
    assert_eq!(authorization.redirect_url, LOOPBACK);
    assert!(server.registrations.lock().unwrap().is_empty());
    let grants = server.token_requests.lock().unwrap().clone();
    assert_eq!(
        form_get(&grants[0], "client_id"),
        "https://app.example/oauth/client.json"
    );
    let state = options
        .store
        .load()
        .await
        .expect("stored state")
        .expect("the sign-in stored its state");
    assert!(state.client_information.is_none());
    close_servers();
}

#[tokio::test]
async fn uses_a_document_and_redirect_uri_specific_to_the_mcp_server_without_the_iss_parameter() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions {
        cimd: true,
        ..Default::default()
    })
    .await;
    let options = loopback_options(&server);
    let authorization = sign_in(&options).await;
    let id = support::callback_id(&server.server_url);
    assert_eq!(
        url::Url::parse(&authorization.authorization_url)
            .ok()
            .and_then(|url| {
                url.query_pairs()
                    .find(|(key, _)| key == "client_id")
                    .map(|(_, value)| value.to_string())
            })
            .as_deref(),
        Some(format!("https://app.example/oauth/{id}/client.json").as_str())
    );
    assert_eq!(authorization.redirect_url, format!("{LOOPBACK}/{id}"));
    let grants = server.token_requests.lock().unwrap().clone();
    assert_eq!(
        form_get(&grants[0], "redirect_uri"),
        format!("{LOOPBACK}/{id}")
    );
    close_servers();
}

#[tokio::test]
async fn appends_the_server_specific_path_without_doubling_a_trailing_slash() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions {
        cimd: true,
        ..Default::default()
    })
    .await;
    let options = McpSignInOptions {
        redirect_url: format!("{LOOPBACK}/"),
        client_metadata_documents: Some(Arc::new(AllDocuments)),
        ..sign_in_options(&server, LOOPBACK)
    };
    let authorization = start_redirect(&options).await;
    let id = support::callback_id(&server.server_url);
    assert_eq!(authorization.redirect_url, format!("{LOOPBACK}/{id}"));
    close_servers();
}

/// Documents that list every redirect URI.
struct AllDocuments;

impl McpClientMetadataDocuments for AllDocuments {
    fn base_url(&self) -> String {
        "https://app.example/oauth".to_string()
    }

    fn lists_redirect_url(&self, _redirect_url: &str) -> bool {
        true
    }
}

#[tokio::test]
async fn registers_dynamically_when_the_server_does_not_support_documents() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions {
        iss_parameter: true,
        ..Default::default()
    })
    .await;
    let options = loopback_options(&server);
    sign_in(&options).await;
    assert_eq!(server.registrations.lock().unwrap().len(), 1);
    let authorizations = server.authorizations.lock().unwrap().clone();
    assert_eq!(
        authorizations[0]
            .iter()
            .find(|(key, _)| key == "client_id")
            .map(|(_, value)| value.clone()),
        Some("client-1".to_string())
    );
    close_servers();
}

#[tokio::test]
async fn registers_dynamically_when_the_documents_do_not_list_the_redirect_uri() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions {
        cimd: true,
        iss_parameter: true,
    })
    .await;
    let options = McpSignInOptions {
        redirect_url: ROUTE.to_string(),
        ..loopback_options(&server)
    };
    sign_in(&options).await;
    assert_eq!(server.registrations.lock().unwrap().len(), 1);
    close_servers();
}

#[tokio::test]
async fn registers_dynamically_without_documents_or_with_a_client_name() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions {
        cimd: true,
        iss_parameter: true,
    })
    .await;
    let without_documents = McpSignInOptions {
        client_metadata_documents: None,
        ..loopback_options(&server)
    };
    sign_in(&without_documents).await;

    let named = McpSignInOptions {
        settings: McpOAuthSettings {
            client_name: Some("Other".to_string()),
            ..Default::default()
        },
        ..loopback_options(&server)
    };
    sign_in(&named).await;
    let registrations = server.registrations.lock().unwrap().clone();
    let names: Vec<_> = registrations
        .iter()
        .map(|registration| {
            registration
                .get("client_name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert_eq!(names, vec!["app".to_string(), "Other".to_string()]);
    close_servers();
}

#[tokio::test]
async fn refreshes_the_tokens_of_a_document_that_lists_only_an_https_route() {
    let _turn = suite_turn().await;
    for client_registration in [ClientRegistration::Auto, ClientRegistration::Cimd] {
        let server = start_authorization_server(AuthorizationServerOptions {
            cimd: true,
            iss_parameter: true,
        })
        .await;
        let options = McpSignInOptions {
            client_metadata_documents: Some(Arc::new(RouteDocuments)),
            settings: McpOAuthSettings {
                client_registration: Some(client_registration),
                ..Default::default()
            },
            ..sign_in_options(&server, ROUTE)
        };
        let authorization = sign_in(&options).await;
        assert_eq!(
            url::Url::parse(&authorization.authorization_url)
                .ok()
                .and_then(|url| {
                    url.query_pairs()
                        .find(|(key, _)| key == "client_id")
                        .map(|(_, value)| value.to_string())
                })
                .as_deref(),
            Some("https://app.example/oauth/client.json")
        );
        let state = options
            .store
            .load()
            .await
            .expect("stored state")
            .expect("the sign-in stored its state");
        assert!(state.client_information.is_none());

        assert_eq!(
            start_mcp_sign_in(options.clone())
                .await
                .expect("startMcpSignIn"),
            McpSignInStart::Authorized
        );
        assert_eq!(server.authorizations.lock().unwrap().len(), 1);
        let grants = server.token_requests.lock().unwrap().clone();
        assert_eq!(form_get(&grants[1], "grant_type"), "refresh_token");
        assert_eq!(
            form_get(&grants[1], "client_id"),
            "https://app.example/oauth/client.json"
        );
        let state = options
            .store
            .load()
            .await
            .expect("stored state")
            .expect("the sign-in stored its state");
        assert_eq!(
            state.tokens.map(|tokens| tokens.access_token).as_deref(),
            Some("access-2")
        );
        close_servers();
    }
}

#[tokio::test]
async fn fails_with_cimd_when_the_server_does_not_support_documents() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions {
        iss_parameter: true,
        ..Default::default()
    })
    .await;
    let options = McpSignInOptions {
        settings: McpOAuthSettings {
            client_registration: Some(ClientRegistration::Cimd),
            ..Default::default()
        },
        ..loopback_options(&server)
    };
    let error = start_mcp_sign_in(options)
        .await
        .expect_err("cimd without support");
    assert!(
        error
            .to_string()
            .contains("does not support Client ID Metadata Documents"),
        "error was {error}"
    );
    assert!(server.registrations.lock().unwrap().is_empty());
    close_servers();
}

#[tokio::test]
async fn fails_with_cimd_when_the_documents_do_not_list_the_redirect_uri() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions {
        cimd: true,
        iss_parameter: true,
    })
    .await;
    let options = McpSignInOptions {
        redirect_url: ROUTE.to_string(),
        settings: McpOAuthSettings {
            client_registration: Some(ClientRegistration::Cimd),
            ..Default::default()
        },
        ..loopback_options(&server)
    };
    let error = start_mcp_sign_in(options)
        .await
        .expect_err("cimd with an unlisted redirect URI");
    assert!(
        error
            .to_string()
            .contains("needs Client ID Metadata Documents that list the redirect URI"),
        "error was {error}"
    );
    assert!(server.registrations.lock().unwrap().is_empty());
    close_servers();
}

#[tokio::test]
async fn cancels_a_start_whose_request_does_not_answer() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions::default()).await;
    server.stall.lock().unwrap().insert("/register".to_string());
    let controller = pi_rs_mcp::AbortController::new();
    let options = McpSignInOptions {
        signal: Some(controller.signal()),
        ..sign_in_options(&server, ROUTE)
    };
    let started = tokio::spawn(async move { start_mcp_sign_in(options).await });
    // The upstream test aborts after 50ms of a stalled register.
    let abort_signal = controller.signal();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        abort_signal.abort_with(None);
    });
    let error = started
        .await
        .expect("join start")
        .expect_err("the stalled register was cancelled");
    assert!(
        matches!(error, OAuthError::SignInCancelled),
        "error was {error:?}"
    );
    close_servers();
}

#[tokio::test]
async fn cancels_a_completion_whose_token_request_does_not_answer() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions::default()).await;
    let options = sign_in_options(&server, ROUTE);
    let authorization = start_redirect(&options).await;
    let location = support::authorize_in_browser(&authorization.authorization_url).await;
    let response = route_response(&location);
    server.stall.lock().unwrap().insert("/token".to_string());
    let controller = pi_rs_mcp::AbortController::new();
    let signal = controller.signal();
    let completed = tokio::spawn(async move {
        let options = McpSignInOptions {
            signal: Some(signal),
            ..options
        };
        complete_mcp_sign_in(options, authorization, response).await
    });
    // The upstream test aborts after 50ms of a stalled token request.
    let abort_signal = controller.signal();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        abort_signal.abort_with(None);
    });
    let error = completed
        .await
        .expect("join completion")
        .expect_err("the stalled token request was cancelled");
    assert!(
        matches!(error, OAuthError::SignInCancelled),
        "error was {error:?}"
    );
    close_servers();
}

#[tokio::test]
async fn cancels_at_once_while_another_process_holds_the_refresh_lock_and_refreshes_nothing() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions::default()).await;
    let recording = recording_store(None);
    let options = McpSignInOptions {
        store: recording.as_server_store(),
        ..sign_in_options(&server, ROUTE)
    };
    sign_in(&options).await;

    let release = recording.hold().await;
    let controller = pi_rs_mcp::AbortController::new();
    let signal = controller.signal();
    let options = McpSignInOptions {
        signal: Some(signal),
        ..options
    };
    let started = tokio::spawn(async move { start_mcp_sign_in(options).await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    controller.abort(None);
    let error = started
        .await
        .expect("join start")
        .expect_err("cancelled while the lock is held");
    assert!(
        matches!(error, OAuthError::SignInCancelled),
        "error was {error:?}"
    );

    // The lock arrives after the cancellation: nothing runs under it, and it is released.
    release();
    wait_lock_runs(&recording).await;
    assert!(!recording.is_locked());
    let grants = server.token_requests.lock().unwrap().clone();
    assert!(
        grants
            .iter()
            .all(|grant| form_get(grant, "grant_type") == "authorization_code")
    );
    assert_eq!(server.authorizations.lock().unwrap().len(), 1);
    close_servers();
}

#[tokio::test]
async fn fails_at_once_with_an_aborted_signal() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions::default()).await;
    let controller = pi_rs_mcp::AbortController::new();
    controller.abort(None);
    let options = McpSignInOptions {
        signal: Some(controller.signal()),
        ..sign_in_options(&server, ROUTE)
    };
    let error = start_mcp_sign_in(options)
        .await
        .expect_err("aborted before the start");
    assert!(
        matches!(error, OAuthError::SignInCancelled),
        "error was {error:?}"
    );
    assert!(server.registrations.lock().unwrap().is_empty());
    close_servers();
}
