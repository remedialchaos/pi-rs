//! `packages/mcp/test/loopback.test.ts`.

use std::sync::Arc;

use pi_rs_mcp::oauth::{
    LoopbackSignInOptions, McpLoopbackCallback, McpOAuthSettings, McpSignInRedirect, OAuthCallback,
    OAuthError, complete_mcp_sign_in, listen_for_loopback_sign_in, response_from_redirect_url,
    start_mcp_sign_in, wait_for_loopback_sign_in,
};

#[path = "support/mod.rs"]
#[expect(
    dead_code,
    reason = "shared test helpers: each integration-test binary uses a different subset"
)]
mod support;

use pi_rs_mcp::oauth::WaitForLoopbackOptions;
use support::{AuthorizationServerOptions, memory_store, start_authorization_server, suite_turn};

/// A pending browser step for a fixed redirect URL.
fn pending(redirect_url: &str) -> McpSignInRedirect {
    McpSignInRedirect {
        kind: "redirect".to_string(),
        authorization_url: "https://auth.example/authorize".to_string(),
        redirect_url: redirect_url.to_string(),
        state: "state-1".to_string(),
        scope: None,
    }
}

/// The loopback callback of one test, closed when dropped.
async fn listen_at(server_url: &str, documents: bool) -> McpLoopbackCallback {
    let settings = McpOAuthSettings::default();

    listen_for_loopback_sign_in(LoopbackSignInOptions {
        server_url,
        store: memory_store(),
        settings: &settings,
        client_metadata_documents: documents.then(|| {
            Arc::new(AllDocuments) as Arc<dyn pi_rs_mcp::oauth::McpClientMetadataDocuments>
        }),
        render_page: None,
    })
    .await
    .expect("listenForLoopbackSignIn")
}

/// Documents that list every redirect URI, like the tests' `listsRedirectUrl: () => true`.
struct AllDocuments;

impl pi_rs_mcp::oauth::McpClientMetadataDocuments for AllDocuments {
    fn base_url(&self) -> String {
        "https://app.example/oauth".to_string()
    }

    fn lists_redirect_url(&self, _redirect_url: &str) -> bool {
        true
    }
}

#[tokio::test]
async fn completes_through_the_loopback_callback_server() {
    let _turn = suite_turn().await;
    let server = start_authorization_server(AuthorizationServerOptions::default()).await;
    let store = memory_store();
    let settings = McpOAuthSettings::default();
    let loopback = listen_for_loopback_sign_in(LoopbackSignInOptions {
        server_url: &server.server_url,
        store: store.clone(),
        settings: &settings,
        client_metadata_documents: None,
        render_page: None,
    })
    .await
    .expect("listenForLoopbackSignIn");
    let redirect_url = loopback.redirect_url.clone();
    assert!(
        redirect_url.starts_with("http://127.0.0.1:") && redirect_url.ends_with("/callback"),
        "redirect URL was {redirect_url}"
    );

    let authorization = match start_mcp_sign_in(pi_rs_mcp::oauth::McpSignInOptions {
        server_url: server.server_url.clone(),
        store: store.clone(),
        settings: McpOAuthSettings::default(),
        client_name: "app".to_string(),
        client_metadata_documents: None,
        redirect_url: redirect_url.clone(),
        challenge: None,
        signal: None,
    })
    .await
    .expect("startMcpSignIn")
    {
        pi_rs_mcp::oauth::McpSignInStart::Authorized => panic!("Expected the browser step"),
        pi_rs_mcp::oauth::McpSignInStart::Redirect(redirect) => redirect,
    };
    let response = {
        let callback = loopback.callback.clone();
        let waiting_authorization = authorization.clone();
        let waiting = tokio::spawn(async move {
            wait_for_loopback_sign_in(WaitForLoopbackOptions {
                callback,
                authorization: waiting_authorization,
                prompt_for_redirect_url: None,
                signal: None,
            })
            .await
        });
        // The browser follows the authorization URL to the callback server.
        let location = support::authorize_in_browser(&authorization.authorization_url).await;
        let client = reqwest::Client::new();
        let page = client
            .get(&location)
            .send()
            .await
            .expect("the callback page loads");
        assert!(page.status().is_success());
        let _ = page.text().await;
        waiting
            .await
            .expect("join wait")
            .expect("waitForLoopbackSignIn")
    };
    complete_mcp_sign_in(
        pi_rs_mcp::oauth::McpSignInOptions {
            server_url: server.server_url.clone(),
            store: store.clone(),
            settings: McpOAuthSettings::default(),
            client_name: "app".to_string(),
            client_metadata_documents: None,
            redirect_url: redirect_url.clone(),
            challenge: None,
            signal: None,
        },
        authorization,
        response,
    )
    .await
    .expect("completeMcpSignIn");

    let state = store
        .load()
        .await
        .expect("stored state")
        .expect("the sign-in stored its state");
    assert_eq!(
        state
            .tokens
            .as_ref()
            .map(|tokens| tokens.access_token.as_str()),
        Some("access-1")
    );
    let registrations = server.registrations.lock().unwrap().clone();
    assert_eq!(registrations.len(), 1);
    let registered = registrations[0]
        .get("redirect_uris")
        .and_then(|uris| uris.as_array().cloned())
        .unwrap_or_default();
    assert_eq!(
        registered.first().and_then(|uri| uri.as_str()),
        Some(redirect_url.as_str()),
        "the registered redirect URI was the loopback one"
    );
    assert_eq!(
        registrations[0]
            .get("application_type")
            .and_then(|t| t.as_str()),
        Some("native")
    );
    loopback.callback.close().await;
    support::close_servers();
}

#[tokio::test]
async fn rejects_a_response_on_another_redirect_path() {
    let _turn = suite_turn().await;
    let server_url = "https://mcp.example/mcp";
    let loopback = listen_at(server_url, true).await;
    let redirect_url = loopback.redirect_url.clone();
    let id = support::callback_id(server_url);
    let authorization = McpSignInRedirect {
        redirect_url: format!("{redirect_url}/{id}"),
        ..pending(&redirect_url)
    };
    let callback = loopback.callback.clone();
    let rejected = tokio::spawn(async move {
        wait_for_loopback_sign_in(WaitForLoopbackOptions {
            callback,
            authorization,
            prompt_for_redirect_url: None,
            signal: None,
        })
        .await
    });
    // A mixed-up authorization server redirects to the shared callback path.
    let client = reqwest::Client::new();
    let page = client
        .get(format!("{redirect_url}?code=c&state=state-1"))
        .send()
        .await
        .expect("the mixed-up redirect loads");
    assert!(page.status().is_success() || page.status().as_u16() == 400);
    let error = rejected
        .await
        .expect("join wait")
        .expect_err("the response arrived on another path");
    assert_eq!(
        error,
        OAuthError::Message(
            "The authorization response arrived on another redirect URI".to_string()
        )
    );
    loopback.callback.close().await;
    support::close_servers();
}

#[tokio::test]
async fn takes_a_pasted_redirect_url_while_waiting_for_the_callback() {
    let _turn = suite_turn().await;
    let loopback = listen_at("https://mcp.example/mcp", false).await;
    let redirect_url = loopback.redirect_url.clone();
    let pasted_redirect = redirect_url.clone();
    let response = wait_for_loopback_sign_in(WaitForLoopbackOptions {
        callback: loopback.callback.clone(),
        authorization: pending(&redirect_url),
        prompt_for_redirect_url: Some({
            let pasted_redirect = pasted_redirect.clone();
            Arc::new(move |_signal| {
                let pasted_redirect = pasted_redirect.clone();
                Box::pin(async move { Ok(format!("{pasted_redirect}?code=pasted&state=state-1")) })
            })
        }),
        signal: None,
    })
    .await
    .expect("waitForLoopbackSignIn");
    assert_eq!(
        response,
        OAuthCallback {
            code: "pasted".to_string(),
            state: "state-1".to_string(),
            iss: None,
        }
    );
    loopback.callback.close().await;
    support::close_servers();
}

#[tokio::test]
async fn cancels_the_wait_and_an_empty_paste_cancels_too() {
    let _turn = suite_turn().await;
    let loopback = listen_at("https://mcp.example/mcp", false).await;
    let redirect_url = loopback.redirect_url.clone();
    let controller = pi_rs_mcp::AbortController::new();
    let waiting = tokio::spawn({
        let callback = loopback.callback.clone();
        let signal = controller.signal();
        let waiting_redirect = redirect_url.clone();
        async move {
            wait_for_loopback_sign_in(WaitForLoopbackOptions {
                callback,
                authorization: pending(&waiting_redirect),
                prompt_for_redirect_url: None,
                signal: Some(signal),
            })
            .await
        }
    });
    controller.abort(None);
    let error = waiting
        .await
        .expect("join wait")
        .expect_err("the wait was cancelled");
    assert!(
        matches!(error, OAuthError::SignInCancelled),
        "error was {error:?}"
    );

    let pasted = wait_for_loopback_sign_in(WaitForLoopbackOptions {
        callback: loopback.callback.clone(),
        authorization: McpSignInRedirect {
            state: "state-2".to_string(),
            ..pending(&redirect_url)
        },
        prompt_for_redirect_url: Some(Arc::new(|_signal| {
            Box::pin(async move { Ok(String::new()) })
        })),
        signal: None,
    })
    .await
    .expect_err("an empty paste cancels");
    assert!(
        matches!(pasted, OAuthError::SignInCancelled),
        "error was {pasted:?}"
    );
    loopback.callback.close().await;
    support::close_servers();
}

#[tokio::test]
async fn reads_the_code_state_and_issuer_from_a_pasted_url() {
    let _turn = suite_turn().await;
    let authorization = pending("http://127.0.0.1:1234/callback/server-id");
    let redirect_url = authorization.redirect_url.clone();
    let response = response_from_redirect_url(
        &format!(" {redirect_url}?code=c&state=state-1&iss=x "),
        &authorization,
    )
    .expect("the pasted URL parses");
    assert_eq!(
        response,
        OAuthCallback {
            code: "c".to_string(),
            state: "state-1".to_string(),
            iss: Some("x".to_string())
        }
    );
}

#[tokio::test]
async fn rejects_another_state_another_redirect_path_and_errors() {
    let _turn = suite_turn().await;
    let authorization = pending("http://127.0.0.1:1234/callback/server-id");
    let redirect_url = authorization.redirect_url.clone();
    let error = response_from_redirect_url(
        &format!("{redirect_url}?code=c&state=other"),
        &authorization,
    )
    .expect_err("another state");
    assert_eq!(
        error,
        OAuthError::Message("The redirect URL belongs to a different sign-in".to_string())
    );
    let error = response_from_redirect_url(
        "http://127.0.0.1:1234/callback?code=c&state=state-1",
        &authorization,
    )
    .expect_err("another redirect path");
    assert_eq!(
        error,
        OAuthError::Message(
            "The redirect URL does not match this sign-in's redirect URI".to_string()
        )
    );
    let error = response_from_redirect_url(
        &format!("{redirect_url}?error=access_denied"),
        &authorization,
    )
    .expect_err("an error response");
    assert_eq!(error, OAuthError::Message("access_denied".to_string()));
    let error = response_from_redirect_url("code=c", &authorization).expect_err("not a URL");
    assert_eq!(
        error,
        OAuthError::Message(
            "Expected the full redirect URL from the browser address bar".to_string()
        )
    );
}
