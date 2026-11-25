use axum::{
    Form, Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use pi_rs_p2_mac::{
    Error,
    brain_directory::BrainDirectory,
    local_sign_in,
    radius_account::RadiusAccount,
    server_config::{Environment, Launch},
    view_cookie,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use url::Url;
mod support;
use support::{Loopback, Test, sandbox};

#[derive(Clone)]
struct Local {
    port: u16,
    mismatch: bool,
}
async fn challenge(
    State(state): State<Local>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
        != Some(&format!("http://127.0.0.1:{}", state.port))
        || body != json!({})
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    Json(json!({"challenge":"fixed challenge"})).into_response()
}
async fn claim(
    State(state): State<Local>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let orig = format!("http://127.0.0.1:{}", state.port);
    let proof = <hmac::Hmac<Sha256> as hmac::Mac>::new_from_slice(b"fixture local owner key")
        .ok()
        .map(|mut mac| {
            hmac::Mac::update(
                &mut mac,
                format!("p2-local-owner:{}:fixed challenge", state.port).as_bytes(),
            );
            hex::encode(hmac::Mac::finalize(mac).into_bytes())
        });
    if state.mismatch
        || headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) != Some(orig.as_str())
        || body["challenge"] != "fixed challenge"
        || proof.as_deref() != body["proof"].as_str()
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let cookie = format!(
        "p2_owner_{}=owner; Path=/; HttpOnly; SameSite=Strict; Max-Age=28800",
        state.port
    );
    (
        StatusCode::OK,
        [
            (header::SET_COOKIE, String::from("p2_other=ignored")),
            (header::SET_COOKIE, cookie),
        ],
    )
        .into_response()
}

#[tokio::test]
async fn owner_challenge_posts_origin_and_consumes_only_the_ports_http_only_cookie() -> Test {
    let dir = sandbox()?;
    fs::write(dir.path().join("owner-key"), " fixture local owner key\n")?;
    assert_eq!(
        local_sign_in::proof("fixture local owner key", 8788, "fixed challenge"),
        "72cd9bcb85f47f944340f23b4479c476a7b28217fed149d9f747ca1676358324"
    );
    for mismatch in [false, true] {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        let address = Url::parse(&format!("http://127.0.0.1:{port}/"))?;
        let router = Router::new()
            .route("/owner/local/challenge", post(challenge))
            .route("/owner/local/claim", post(claim))
            .with_state(Local { port, mismatch });
        let app = tokio::spawn(async move { axum::serve(listener, router).await });
        let result = local_sign_in::cookie(&address, dir.path()).await;
        app.abort();
        if mismatch {
            assert!(
                matches!(result,Err(Error::Protocol(s)) if s.contains("does not use the data directory"))
            );
        } else {
            let cookie = result?;
            assert_eq!(cookie.cookie.name(), format!("p2_owner_{port}"));
            assert_eq!(cookie.cookie.value(), "owner");
            assert_eq!(cookie.cookie.path(), Some("/"));
            assert_eq!(cookie.cookie.http_only(), Some(true));
            assert_eq!(cookie.cookie.same_site(), Some(cookie::SameSite::Strict));
            assert_eq!(
                cookie.cookie.max_age().map(|d| d.whole_seconds()),
                Some(28800)
            );
        }
    }
    assert!(
        local_sign_in::cookie(&Url::parse("https://example.invalid/")?, dir.path())
            .await
            .is_err()
    );
    Ok(())
}

struct Gateway {
    base: Url,
    bad_state: bool,
    authorize: Mutex<Option<HashMap<String, String>>>,
    refreshes: AtomicUsize,
    opens: AtomicUsize,
}
async fn discovery(State(g): State<Arc<Gateway>>) -> Json<Value> {
    Json(json!({"authorizationEndpoint":format!("{}oauth/authorize",g.base)}))
}
async fn authorize(
    State(g): State<Arc<Gateway>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let values = [
        ("client_id", "pi-gateway"),
        ("scope", "gateway offline_access"),
        ("response_type", "code"),
        ("code_challenge_method", "S256"),
        ("handoff", "url"),
    ];
    if values
        .iter()
        .any(|(k, v)| query.get(*k).map(String::as_str) != Some(*v))
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(mut url) = query
        .get("redirect_uri")
        .and_then(|uri| Url::parse(uri).ok())
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if url.host_str() != Some("127.0.0.1")
        || url.path() != "/oauth/callback"
        || url.port().is_none()
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(state) = query.get("state") else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    url.query_pairs_mut()
        .append_pair("code", "auth+code=/")
        .append_pair("state", if g.bad_state { "wrong state" } else { state });
    *g.authorize.lock().unwrap_or_else(|e| e.into_inner()) = Some(query);
    Redirect::temporary(url.as_str()).into_response()
}
async fn tokens(
    State(g): State<Arc<Gateway>>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if form.get("client_id").map(String::as_str) != Some("pi-gateway") {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let get = |key| form.get(key).map(String::as_str);
    match get("grant_type") {
        Some("authorization_code") => {
            let auth = g.authorize.lock().unwrap_or_else(|e| e.into_inner());
            let Some(auth) = auth.as_ref() else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            let Some(verifier) = get("code_verifier") else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            if get("code") != Some("auth+code=/")
                || get("redirect_uri") != auth.get("redirect_uri").map(String::as_str)
                || auth.get("code_challenge")
                    != Some(&URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())))
            {
                return StatusCode::BAD_REQUEST.into_response();
            }
            Json(json!({"access_token":"initial","refresh_token":"f+x","expires_in":3600,"scope":"gateway offline_access"})).into_response()
        }
        Some("refresh_token") if get("refresh_token") == Some("f+x") => {
            g.refreshes.fetch_add(1, Ordering::Relaxed);
            Json(json!({"access_token":"fresh","refresh_token":"f+x","expires_in":3600}))
                .into_response()
        }
        _ => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid_grant"})),
        )
            .into_response(),
    }
}
async fn servers(headers: HeaderMap) -> Response {
    let accepted = |token| {
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            == Some(token)
    };
    if accepted("Bearer disabled") {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"pi_shell_not_enabled"})),
        )
            .into_response();
    }
    if !accepted("Bearer fresh") {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error":"expired"}))).into_response();
    }
    Json(json!({"servers":[{"serverId":"same-mac","name":"Mine","status":"online","organization":{"name":"Work","id":"org"}}]})).into_response()
}
async fn open_server(
    State(g): State<Arc<Gateway>>,
    Path((org, id)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if org != "org"
        || id != "server/with slash"
        || query.get("path").map(String::as_str) != Some("/files?what=a+b#heading")
        || headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            != Some("Bearer fresh")
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    g.opens.fetch_add(1, Ordering::Relaxed);
    let url = g.base.join("grant?ticket=one%2Btwo&path=%2Ffiles");
    match url {
        Ok(url) => Json(json!({"url":url.as_str()})).into_response(),
        Err(_) => StatusCode::BAD_REQUEST.into_response(),
    }
}
async fn exchange(Query(query): Query<HashMap<String, String>>, headers: HeaderMap) -> Response {
    let header = |key| headers.get(key).and_then(|h| h.to_str().ok());
    if query.len() != 1
        || query.get("ticket").map(String::as_str) != Some("one+two")
        || header("Sec-Fetch-Mode") != Some("navigate")
        || header("Sec-Fetch-Dest") != Some("document")
        || header("Sec-Fetch-Site") != Some("none")
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    (
        StatusCode::NO_CONTENT,
        [(
            header::SET_COOKIE,
            "p2_view=opaque; Path=/; HttpOnly; Secure; SameSite=Lax",
        )],
    )
        .into_response()
}

#[tokio::test]
async fn radius_does_pkce_state_token_refresh_in_place_and_view_cookie_exchange() -> Test {
    for bad_state in [false, true] {
        let dir = sandbox()?;
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let base = Url::parse(&format!("http://{}/", listener.local_addr()?))?;
        let state = Arc::new(Gateway {
            base: base.clone(),
            bad_state,
            authorize: Mutex::new(None),
            refreshes: AtomicUsize::new(0),
            opens: AtomicUsize::new(0),
        });
        let gateway = Router::new()
            .route("/v1/oauth", get(discovery))
            .route("/oauth/authorize", get(authorize))
            .route("/v1/oauth/token", post(tokens))
            .route("/v1/pi/servers", get(servers))
            .route("/v1/pi/servers/{org}/{id}/open", post(open_server))
            .route("/grant", get(exchange))
            .with_state(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, gateway).await });
        let result = exercise_gateway(dir.path(), base, &state).await;
        server.abort();
        result?;
    }
    Ok(())
}

async fn exercise_gateway(dir: &std::path::Path, base: Url, state: &Gateway) -> Test {
    let account = RadiusAccount::new(dir, base.clone())?;
    let config = dir.join("radius.json");
    fs::write(
        &config,
        serde_json::to_vec(
            &json!({"gateway":base.as_str(),"serverId":"same-mac","label":"existing"}),
        )?,
    )?;
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        account.sign_in(true, |_| {
            Err(Error::Protocol("unexpected browser launch".into()))
        }),
    )
    .await?;
    if state.bad_state {
        assert!(
            matches!(result,Err(Error::Radius(0,reason)) if reason=="the sign-in answered for another request")
        );
        let stored: Value = serde_json::from_slice(&fs::read(&config)?)?;
        assert!(stored.get("credential").is_none());
        return Ok(());
    }
    result?;
    assert!(account.signed_in());
    let first: Value = serde_json::from_slice(&fs::read(&config)?)?;
    assert_eq!(first["serverId"], "same-mac");
    assert_eq!(first["label"], "existing");
    assert_eq!(first["credential"]["access"], "initial");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(config.metadata()?.permissions().mode() & 0o777, 0o600);
    }
    let env = Environment::from([("P2MAC_DATA_DIR".into(), dir.to_string_lossy().into())]);
    let server = Launch::load(&env, &std::collections::BTreeMap::new(), None)?.selected;
    let mut directory = BrainDirectory::new(None);
    directory.poll(&account, &server, true).await?;
    assert!(directory.shows_tabs);
    assert_eq!(directory.brains[0].id, "same-mac");
    assert!(directory.brains[0].is_direct());
    assert_eq!(state.refreshes.load(Ordering::Relaxed), 1);
    let mut latest: Value = serde_json::from_slice(&fs::read(&config)?)?;
    assert_eq!(latest["credential"]["access"], "fresh");
    assert_eq!(latest["serverId"], "same-mac");
    assert_eq!(latest["label"], "existing");
    let grant = account
        .open_url("org", "server/with slash", "/files?what=a+b#heading")
        .await?;
    let cookie = view_cookie::exchange(&grant).await?;
    assert_eq!(cookie.cookie.name_value(), ("p2_view", "opaque"));
    assert_eq!(cookie.cookie.secure(), Some(true));
    assert_eq!(state.opens.load(Ordering::Relaxed), 1);
    assert_eq!(view_cookie::RENEW_EVERY, Duration::from_secs(21600));
    assert_eq!(view_cookie::RETRY_AFTER, Duration::from_secs(300));
    assert_eq!(
        view_cookie::forward_path(&base, &base.join("pi/forward?path=%2Fapp%3Fa%3D2")?).as_deref(),
        Some("/app?a=2")
    );
    assert_eq!(
        view_cookie::forward_path(
            &base,
            &Url::parse("https://foreign.invalid/pi/forward?path=%2Fapp")?
        ),
        None
    );
    latest["credential"]["access"] = json!("disabled");
    fs::write(&config, serde_json::to_vec(&latest)?)?;
    directory.poll(&account, &server, true).await?;
    assert!(!directory.shows_tabs);
    account.sign_out()?;
    assert!(!account.signed_in());
    assert!(!config.exists());
    Ok(())
}

#[tokio::test]
async fn grant_exchange_refuses_redirects_and_missing_set_cookie() -> Test {
    let server = Loopback::serve(
        Router::new()
            .route("/redirect", get(|| async { Redirect::temporary("/grant") }))
            .route("/grant", get(|| async { StatusCode::NO_CONTENT })),
    )
    .await?;
    for (path, status) in [("redirect", "307"), ("grant", "204")] {
        let Err(error) = view_cookie::exchange(&server.url.join(path)?).await else {
            return Err("accepted grant without cookie".into());
        };
        assert!(error.to_string().contains(status));
    }
    Ok(())
}
