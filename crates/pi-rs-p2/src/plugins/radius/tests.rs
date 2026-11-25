use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    extract::{
        Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{get, post, put},
};
use futures_util::StreamExt;
use pi_rs_ai::auth::{Credential, LoginOptions, OAuthCredential};
use pi_rs_chord::node::http::{RouteOptions, text_response};
use serde_json::{Value, json};
use tokio::{sync::mpsc, time::timeout};
use url::Url;
use uuid::Uuid;

use crate::{
    api::v1::{FacetKind, server::service_of},
    hosts::server::{ServerOptions, start_server},
    plugins::shard_a::PeopleUrl,
    runtime::server::ServerServices,
};

use super::{
    BACKUP_TARGET, CONNECTIONS, RadiusBackupTarget, RadiusConnection, Status,
    brain::{Config, ConfigFile},
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
const WAIT: Duration = Duration::from_secs(10);

enum GatewayEvent {
    Connect(std::collections::HashMap<String, String>),
    Shares(Value),
    Name(Value),
    Frame(Message),
}

#[derive(Clone)]
struct Gateway {
    events: mpsc::Sender<GatewayEvent>,
    input: Arc<tokio::sync::Mutex<mpsc::Receiver<Message>>>,
}

async fn connect(
    State(gateway): State<Gateway>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    ws: WebSocketUpgrade,
    headers: HeaderMap,
) -> std::result::Result<Response, StatusCode> {
    if headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        != Some("Bearer fixture-token")
    {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(ws
        .protocols(["pi-server.host.v4"])
        .on_upgrade(move |socket| gateway_socket(gateway, query, socket)))
}

async fn gateway_socket(
    gateway: Gateway,
    query: std::collections::HashMap<String, String>,
    mut socket: WebSocket,
) {
    let mut input = gateway.input.lock().await;
    if gateway
        .events
        .send(GatewayEvent::Connect(query))
        .await
        .is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            Some(message) = input.recv() => { if socket.send(message).await.is_err() { return; } }
            incoming = socket.next() => match incoming {
                Some(Ok(message)) => { if gateway.events.send(GatewayEvent::Frame(message)).await.is_err() { return; } }
                _ => return,
            },
        }
    }
}

async fn open(
    output: &mpsc::Sender<Message>,
    id: Uuid,
    principal: Value,
    cookie: &str,
    window: usize,
) -> TestResult {
    let message = json!({
        "version":2,"type":"connection_open","connection_id":id,"kind":"http",
        "principal":principal,"public_origin":"https://brain.example.test","window":window,
        "request":{"method":"GET","path":"/test/radius","headers":[
            ["x-radius-shell-origin","https://shell.example.test"],
            ["origin","https://brain.example.test"],["cookie",cookie],
            ["x-p2-relay","forged"],["x-p2-relay-user","forged"]
        ]}
    });
    output
        .send(Message::Text(message.to_string().into()))
        .await?;
    let mut end = vec![2, 3];
    end.extend(id.as_bytes());
    output.send(Message::Binary(end.into())).await?;
    Ok(())
}

#[tokio::test]
async fn relay_authenticates_the_owner_and_streams_with_credit() -> TestResult {
    let (events, mut messages) = mpsc::channel(32);
    let (output, input) = mpsc::channel(16);
    let gateway = Gateway {
        events,
        input: Arc::new(tokio::sync::Mutex::new(input)),
    };
    let router = Router::new()
        .route("/v1/context",get(|| async { Json(json!({"actor":{"principal_kind":"user","principal_id":"owner"},"organization":{"id":"org"}})) }))
        .route("/v1/servers/{id}/connect", get(connect))
        .route("/v1/servers/{id}/shares", put(|State(gateway): State<Gateway>, Json(value): Json<Value>| async move {
            let _ = gateway.events.send(GatewayEvent::Shares(value)).await;
            StatusCode::NO_CONTENT
        }))
        .route("/v1/servers/{id}/name", put(|State(gateway): State<Gateway>, Json(value): Json<Value>| async move {
            let _ = gateway.events.send(GatewayEvent::Name(value)).await;
            StatusCode::NO_CONTENT
        })).with_state(gateway);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let dir = tempfile::tempdir()?;
    let now = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?;
    let server_id = Uuid::new_v4().to_string();
    let file = ConfigFile::new(dir.path());
    file.write(&Config {
        gateway: origin.clone(),
        server_id: server_id.clone(),
        public_origin: None,
        shell_page: Some(format!(
            "https://shell.example.test/pi/brains/org/{server_id}"
        )),
        credential: Credential::OAuth(OAuthCredential {
            access: "fixture-token".into(),
            refresh: "fixture-refresh".into(),
            expires: now + 3_600_000,
            details: Default::default(),
        }),
    })
    .await?;
    let mut options = ServerOptions::new(dir.path().to_owned());
    options.port = 0;
    options.dev = false;
    super::register(&mut options.registry)?;
    let mut routes = ServerServices::default();
    routes.route(
        "GET /test/radius",
        RouteOptions::new(|request| {
            Box::pin(async move {
                let owner = request
                    .principal
                    .as_ref()
                    .is_some_and(|principal| principal["access"] == "relay");
                if !owner {
                    return text_response(403, "not the relay owner");
                }
                if request.headers.contains_key("cookie") {
                    return text_response(400, "browser cookie crossed the relay");
                }
                text_response(200, "hello from the relayed server")
            })
        }),
    )?;
    options
        .registry
        .register("test-radius", FacetKind::Server, routes.facet())?;
    let app = start_server(options).await?;
    let connection = service_of::<RadiusConnection>(&app.host, CONNECTIONS)?;
    let backup = service_of::<RadiusBackupTarget>(&app.host, BACKUP_TARGET)?
        .open()
        .await?;
    assert_eq!(backup.device, server_id);
    assert_eq!(backup.server, format!("server-{server_id}"));
    let mut status = connection.brain.status();
    timeout(
        WAIT,
        status.wait_for(|status| matches!(status, Status::Connected { .. })),
    )
    .await??;
    let urls = service_of::<PeopleUrl>(&app.host, "p2.share-urls")?;
    assert_eq!(
        (urls.0)("one two"),
        Some(format!(
            "https://shell.example.test/pi/org/{server_id}/s/one%20two"
        ))
    );
    let Some(GatewayEvent::Connect(query)) = timeout(WAIT, messages.recv()).await? else {
        return Err("Missing Radius host connection".into());
    };
    assert_eq!(query.get("replace").map(String::as_str), Some("1"));
    connection
        .brain
        .report_shares(vec!["reader@example.test".into()]);
    connection.brain.rename(Some("Test brain".into()));
    let mut updated = (false, false);
    timeout(WAIT, async {
        while !updated.0 || !updated.1 {
            match messages.recv().await {
                Some(GatewayEvent::Shares(value)) => {
                    updated.0 = value == json!({"emails":["reader@example.test"]})
                }
                Some(GatewayEvent::Name(value)) => {
                    updated.1 = value == json!({"name":"Test brain"})
                }
                None => break,
                _ => {}
            }
        }
    })
    .await?;
    assert_eq!(updated, (true, true));
    let denied_id = Uuid::new_v4();
    open(
        &output,
        denied_id,
        json!({"kind":"user","id":"owner","organization_id":"other"}),
        "claimed=owner",
        4,
    )
    .await?;
    let Some(GatewayEvent::Frame(Message::Text(denied))) = timeout(WAIT, messages.recv()).await?
    else {
        return Err("Missing relay denial".into());
    };
    let denied: Value = serde_json::from_str(denied.as_str())?;
    assert_eq!(denied["type"], "connection_close");
    assert_eq!(denied["connection_id"], denied_id.to_string());
    assert_eq!(denied["code"], 1008);
    let id = Uuid::new_v4();
    open(
        &output,
        id,
        json!({"kind":"user","id":"owner","organization_id":"org"}),
        "local=owner",
        4,
    )
    .await?;
    let Some(GatewayEvent::Frame(Message::Text(head))) = timeout(WAIT, messages.recv()).await?
    else {
        return Err("Missing response headers".into());
    };
    let head: Value = serde_json::from_str(head.as_str())?;
    assert_eq!(head["status"], 200);
    let Some(GatewayEvent::Frame(Message::Binary(first))) = timeout(WAIT, messages.recv()).await?
    else {
        return Err("Missing response body".into());
    };
    assert_eq!(&first[18..], b"hell");
    assert!(
        timeout(Duration::from_millis(100), messages.recv())
            .await
            .is_err()
    );
    let credit = json!({"version":2,"type":"credit","connection_id":id,"bytes":64});
    output
        .send(Message::Text(credit.to_string().into()))
        .await?;
    let Some(GatewayEvent::Frame(Message::Binary(last))) = timeout(WAIT, messages.recv()).await?
    else {
        return Err("Missing response after credit".into());
    };
    assert_eq!(&last[18..], b"o from the relayed server");
    let Some(GatewayEvent::Frame(Message::Binary(end))) = timeout(WAIT, messages.recv()).await?
    else {
        return Err("Missing response end".into());
    };
    assert_eq!(end.len(), 18);
    assert_eq!(end[1], 3);
    let saved = file.read().await?.ok_or("Missing Radius config")?;
    assert_eq!(
        saved.public_origin.as_deref(),
        Some("https://brain.example.test")
    );
    assert_eq!(
        saved.shell_page,
        Some(format!("https://shell.example.test/pi/org/{server_id}"))
    );
    app.close().await?;
    server.abort();
    Ok(())
}

#[tokio::test]
async fn device_login_persists_oauth_and_preserves_the_gateway_identity() -> TestResult {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let gateway = Url::parse(&format!("http://{}", listener.local_addr()?))?;
    let directory = tempfile::tempdir()?;
    let reported = Arc::new(Mutex::new(Vec::new()));
    let router = Router::new()
        .route("/v1/oauth/device",post(|| async { Json(json!({
            "device_code":"fixture-device","user_code":"ABCD","verification_uri":"https://radius.example.test/check","expires_in":60,"interval":1
        })) }))
        .route("/v1/oauth/token",post(|body:String| async move {
            if !body.contains("device_code=fixture-device") {
                return (StatusCode::BAD_REQUEST,Json(json!({"error":"invalid_request"})));
            }
            (StatusCode::OK,Json(json!({"access_token":"issued","refresh_token":"renew","expires_in":3600})))
        }));
    let task = tokio::spawn(async move { axum::serve(listener, router).await });
    let options = LoginOptions::default();
    let copy = reported.clone();
    let first = super::login::login(
        directory.path(),
        gateway.clone(),
        move |url, code| {
            copy.lock()
                .unwrap_or_else(|error| error.into_inner())
                .push((url.to_owned(), code.to_owned()));
        },
        &options,
    )
    .await?;
    let file = ConfigFile::new(directory.path());
    let mut original = file.read().await?.ok_or("Missing login state")?;
    original.public_origin = Some("https://brain.example.test".into());
    file.write(&original).await?;
    let second = super::login::login(directory.path(), gateway, |_, _| {}, &options).await?;
    assert_eq!(first, second);
    let saved = file.read().await?.ok_or("Missing persisted login")?;
    assert_eq!(saved.public_origin, original.public_origin);
    assert!(
        matches!(saved.credential,Credential::OAuth(credential) if credential.access == "issued" && credential.refresh == "renew")
    );
    let observed = reported.lock().unwrap_or_else(|error| error.into_inner());
    assert_eq!(
        observed.as_slice(),
        [(
            "https://radius.example.test/check".to_owned(),
            "ABCD".to_owned()
        )]
    );
    task.abort();
    Ok(())
}
