use std::{path::Path, sync::Arc, time::Duration};

use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::post};
use pi_rs_chord::{
    Error,
    context::Context,
    host::{Caller, Held},
};
use serde_json::{Value, json};

use crate::{
    api::v1::p2_contracts::lookup,
    hosts::server::{P2Server, ServerOptions, start_server},
};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

async fn fixture(path: &Path) -> TestResult<P2Server> {
    let mut options = ServerOptions::new(path.to_owned());
    options.port = 0;
    options.dev = false;
    options.fake = true;
    crate::plugins::shard_a::register(&mut options.registry)?;
    super::register(&mut options.registry)?;
    Ok(start_server(options).await?)
}

async fn call(held: &Held, action: &str, args: Value, ctx: &Context) -> pi_rs_chord::Result<Value> {
    held.call(action, args.into(), ctx).await?.to_serde()
}

async fn mcp(Json(request): Json<Value>) -> impl IntoResponse {
    let reply = match request["method"].as_str() {
        Some("initialize") => json!({
            "protocolVersion":request["params"]["protocolVersion"],
            "serverInfo":{"name":"fixture","version":"1"},"capabilities":{"tools":{}},"instructions":"Only for tests"
        }),
        Some("tools/list") => json!({"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}),
        Some("tools/call") => {
            json!({"content":[{"type":"text","text":request["params"]["arguments"]["text"]}]})
        }
        _ => return StatusCode::ACCEPTED.into_response(),
    };
    Json(json!({"jsonrpc":"2.0","id":request["id"],"result":reply})).into_response()
}

#[tokio::test]
async fn leased_http_tool_runs_and_stale_incarnations_cannot_call_it() -> TestResult {
    let endpoint = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = endpoint.local_addr()?;
    let endpoint_task =
        tokio::spawn(
            async move { axum::serve(endpoint, Router::new().route("/mcp", post(mcp))).await },
        );
    let dir = tempfile::tempdir()?;
    tokio::fs::write(
        dir.path().join("mcp.json"),
        json!({"mcpServers":{
            "echo":{"url":format!("http://{address}/mcp"),"description":"Echoes arguments",
                "headers":{"Authorization":"Bearer fixture"}}
        }})
        .to_string(),
    )
    .await?;
    let app = fixture(dir.path()).await?;
    let ctx = Context::background();
    let catalog = app
        .host
        .get(Arc::new(lookup("p2.catalog")?), json!({}).into(), &ctx)
        .await?;
    let id = uuid::Uuid::new_v4().to_string();
    let cwd = dir.path().to_string_lossy();
    call(
        &catalog,
        "create",
        json!({"id":id,"title":"Test","cwd":cwd,
        "model":{"provider":"fake","modelId":"faux-1"},"thinkingLevel":"off"}),
        &ctx,
    )
    .await?;
    let manager = app
        .host
        .get(Arc::new(lookup("p2.mcp.manager")?), json!({}).into(), &ctx)
        .await?;
    let location = json!({"cwd":cwd,"env":null});
    for (generation, incarnation) in [(1, "old"), (2, "current")] {
        call(
            &manager,
            "setLocations",
            json!({"session":id,"generation":generation,"incarnation":incarnation,
            "locations":[location]}),
            &ctx,
        )
        .await?;
    }
    let rows = manager.member("instances")?;
    let connected: Value = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let values: Value = rows.value()?.to_json().to_serde()?;
            if values[0]["status"] == "connected" {
                return Ok::<_, pi_rs_chord::Error>(values);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    assert_eq!(connected[0]["instructions"], "Only for tests");
    assert_eq!(connected[0]["tools"][0]["name"], "echo");
    let mut input = json!({"session":id,"generation":2,"incarnation":"old","location":location,
        "server":"echo","tool":"echo","args":{"text":"from MCP"}});
    assert!(matches!(call(&manager,"call",input.clone(),&ctx).await,
        Err(Error::Chord(error)) if error.code == "forbidden"));
    input["incarnation"] = json!("current");
    let result = call(&manager, "call", input.clone(), &ctx).await?;
    assert_eq!(result["content"][0]["text"], "from MCP");
    call(
        &manager,
        "releaseLeases",
        json!({"session":id,"generation":2}),
        &ctx,
    )
    .await?;
    assert!(call(&manager, "call", input, &ctx).await.is_err());
    drop((manager, catalog));
    app.close().await?;
    endpoint_task.abort();
    Ok(())
}

#[tokio::test]
async fn guest_cannot_hold_owner_mcp_or_start_authentication() -> TestResult {
    let dir = tempfile::tempdir()?;
    let app = fixture(dir.path()).await?;
    let ctx = Context::background();
    let guest = app.host.hold_as(
        Arc::new(lookup("p2.mcp")?),
        json!({}).into(),
        Caller {
            principal: Some(json!({"kind":"guest","id":"guest","name":"Guest","home":"",
            "sessions":{},"revision":1})),
            incarnation: "fixture".into(),
        },
        &ctx,
    )?;
    assert!(guest.wait(&ctx).await.is_err());
    let response =
        reqwest::get(format!("http://{}/mcp/callback?state=unknown", app.address)).await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.text().await?.contains("paste it in p2"));
    drop(guest);
    app.close().await?;
    Ok(())
}

#[derive(Clone)]
struct OAuthFixture {
    origin: String,
    challenge: Arc<std::sync::Mutex<Option<String>>>,
}

#[tokio::test]
async fn pasted_oauth_callback_requires_the_active_state_and_persists_tokens() -> TestResult {
    use axum::extract::State;
    use base64::Engine;
    use sha2::{Digest, Sha256};
    use url::Url;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("http://{}", listener.local_addr()?);
    let state = OAuthFixture {
        origin: origin.clone(),
        challenge: Arc::new(std::sync::Mutex::new(None)),
    };
    let router = Router::new()
        .route("/.well-known/oauth-protected-resource/mcp",axum::routing::get(|State(state): State<OAuthFixture>| async move {
            Json(json!({"resource":format!("{}/mcp",state.origin),"authorization_servers":[state.origin]}))
        }))
        .route("/.well-known/oauth-authorization-server",axum::routing::get(|State(state): State<OAuthFixture>| async move {
            let origin = state.origin;
            Json(json!({"issuer":origin,"authorization_endpoint":format!("{origin}/authorize"),
                "token_endpoint":format!("{origin}/token"),"registration_endpoint":format!("{origin}/register"),
                "response_types_supported":["code"],"code_challenge_methods_supported":["S256"],
                "token_endpoint_auth_methods_supported":["none"]}))
        }))
        .route("/register",post(|Json(mut metadata): Json<Value>| async move {
            metadata["client_id"] = json!("fixture-client");
            (StatusCode::CREATED, Json(metadata))
        }))
        .route("/token", post(|State(state):State<OAuthFixture>, body: String| async move {
            let fields: std::collections::HashMap<_,_> = url::form_urlencoded::parse(body.as_bytes()).collect();
            let verifier = fields.get("code_verifier").map(|value| {
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(value.as_bytes()))
            });
            let challenge = state.challenge.lock().unwrap_or_else(|e| e.into_inner());
            if verifier.is_none() || verifier.as_ref() != challenge.as_ref()
                || fields.get("code").map(|value| value.as_ref()) != Some("issued-code")
            {
                return (StatusCode::BAD_REQUEST,Json(json!({"error":"invalid_grant"})));
            }
            (StatusCode::OK,Json(json!({"access_token":"issued-access","refresh_token":"issued-refresh","token_type":"Bearer"})))
        })).with_state(state.clone());
    let task = tokio::spawn(async move { axum::serve(listener, router).await });
    let dir = tempfile::tempdir()?;
    let credentials = super::credentials::Credentials::new(dir.path());
    let flows = Arc::new(super::sign_in::SignIns::new(
        credentials,
        "p2 fixture".into(),
    ));
    let server = super::placement::OAuthServer {
        name: "docs".into(),
        url: format!("{origin}/mcp"),
        config: None,
    };
    let redirect = flows
        .begin(
            server.clone(),
            Some("https://relay.example.test/mcp/callback".into()),
        )
        .await?;
    assert_eq!(redirect["kind"], "redirect");
    let authorization = Url::parse(
        redirect["url"]
            .as_str()
            .ok_or("missing authorization URL")?,
    )?;
    let fields: std::collections::HashMap<_, _> =
        authorization.query_pairs().into_owned().collect();
    *state.challenge.lock().unwrap_or_else(|e| e.into_inner()) =
        fields.get("code_challenge").cloned();
    let mut callback = Url::parse(fields.get("redirect_uri").ok_or("missing redirect_uri")?)?;
    let id = flows.list().await[0]["id"]
        .as_str()
        .ok_or("missing flow ID")?
        .to_owned();
    callback
        .query_pairs_mut()
        .append_pair("code", "issued-code")
        .append_pair("state", "wrong");
    assert!(flows.paste(&id, callback.as_str()).await.is_err());
    callback.set_query(None);
    callback
        .query_pairs_mut()
        .append_pair("code", "issued-code")
        .append_pair("state", fields.get("state").ok_or("missing state")?);
    flows.paste(&id, callback.as_str()).await?;
    assert_eq!(flows.list().await, json!([]));
    let reopened = super::credentials::Credentials::new(dir.path());
    let saved = reopened
        .server("docs", &server.url)?
        .load()
        .await?
        .ok_or("missing OAuth state")?;
    assert_eq!(
        saved.tokens.ok_or("missing OAuth tokens")?.access_token,
        "issued-access"
    );
    flows.close().await;
    task.abort();
    Ok(())
}
