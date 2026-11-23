use super::*;
use crate::{
    api::v1::{FacetKind, PluginRegistry, Principal, p2_contracts::lookup},
    hosts::server::{P2Server, ServerOptions, start_server},
    runtime::server::server_of,
};
use pi_rs_chord::{
    Error,
    context::Context,
    host::{Members, ServeOptions, define_facet},
    json::JsonValue,
};
use std::{path::Path, sync::Arc};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

fn registry(archived: bool) -> Result<PluginRegistry> {
    let mut registry = PluginRegistry::new();
    super::super::register(&mut registry)?;
    let catalog = Arc::new(lookup("p2.catalog")?);
    registry.register(
        "catalog-fixture",
        FacetKind::Server,
        define_facet(move |facet| {
            facet.serve(
                catalog.clone(),
                move |_, _, object| {
                    let row = JsonValue::from_serde(json!({
                        "id":"one", "cwd":"/", "title":"One", "createdAt":1, "updatedAt":1,
                        "archivedAt":archived.then_some(2), "pinned":false,
                        "model":{"provider":"fake","modelId":"faux-1"}, "thinkingLevel":"off",
                        "env":null, "main":1, "branches":[],
                    }));
                    let mut members = Members::new();
                    members.state(
                        "sessions",
                        object.writable(JsonValue::Array(vec![row.clone()]))?,
                    )?;
                    members.method("get", move |_, _| {
                        let row = row.clone();
                        Box::pin(async move { Ok(row) })
                    })?;
                    for name in ["activity", "create", "update"] {
                        members.method(name, |_, _| {
                            Box::pin(async {
                                Err(Error::Invalid("unexpected catalog fixture call".into()))
                            })
                        })?;
                    }
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    )?;
    Ok(registry)
}

async fn start(path: &Path, registry: &PluginRegistry) -> TestResult<P2Server> {
    let mut options = ServerOptions::new(path.to_path_buf());
    options.port = 0;
    options.fake = true;
    options.dev = false;
    options.registry = registry.clone();
    Ok(start_server(options).await?)
}

#[tokio::test]
async fn persisted_guest_grants_reject_missing_expired_withdrawn_and_stale_access() -> TestResult {
    let directory = tempfile::tempdir()?;
    let definitions = Tables::new()?;
    let active_registry = registry(false)?;
    let authorizers = active_registry.session_authorizers(&active_registry.select(None)?);
    let authorize = authorizers
        .get("sharing")
        .ok_or_else(|| Error::Invalid("sharing authorizer was not registered".into()))?;
    let server = start(directory.path(), &active_registry).await?;
    let guest = "reader";
    let session = "one";
    let address = session_address(session);
    let query = |server: &P2Server, revision, mode| {
        authorize(
            server.host.clone(),
            Principal::Guest {
                id: guest.into(),
                name: "Reader".into(),
                email: None,
                home: session.into(),
                sessions: IndexMap::from([(session.into(), mode)]),
                revision,
            },
            session.into(),
            Context::background(),
        )
    };
    let grant = |link: &str, removed| {
        definitions.grants.put(
            OWNER,
            json!({
                "session": address, "guest": guest, "mode":"view",
                "linkId": link, "removed": removed, "grantedAt": 1,
            }),
        )
    };
    let natives = server_of(&server.host)?;
    let engine = natives.store();
    let shares = server
        .host
        .get(
            Arc::new(lookup(OWNER)?),
            JsonValue::from_serde(json!({})),
            &Context::background(),
        )
        .await?;
    assert!(!query(&server, 2, GrantMode::View).await?, "missing grant");

    engine
        .commit(vec![
            definitions.guests.put(
                OWNER,
                json!({
                    "id":guest, "name":"Reader", "revision":2, "lastSeen":1,
                }),
            )?,
            definitions.links.put(
                OWNER,
                json!({
                    "session":address, "mode":"view", "id":"active", "token":"test",
                }),
            )?,
            grant("old", false)?,
        ])
        .await?;
    assert!(!query(&server, 2, GrantMode::View).await?, "expired link");
    engine.commit(vec![grant("active", true)?]).await?;
    assert!(
        !query(&server, 2, GrantMode::View).await?,
        "withdrawn grant"
    );
    engine.commit(vec![grant("active", false)?]).await?;
    assert!(query(&server, 2, GrantMode::View).await?, "live grant");
    assert!(!query(&server, 1, GrantMode::View).await?, "stale revision");
    assert!(!query(&server, 2, GrantMode::Full).await?, "different mode");
    drop(shares);
    drop(natives);
    server.close().await?;

    let archived = start(directory.path(), &registry(true)?).await?;
    assert!(
        !query(&archived, 2, GrantMode::View).await?,
        "archived session"
    );
    archived.close().await?;

    let restored = start(directory.path(), &active_registry).await?;
    assert!(
        query(&restored, 2, GrantMode::View).await?,
        "reopened grant"
    );
    server_of(&restored.host)?
        .store()
        .commit(vec![grant("active", true)?])
        .await?;
    restored.close().await?;

    let reopened = start(directory.path(), &active_registry).await?;
    assert!(
        !query(&reopened, 2, GrantMode::View).await?,
        "persisted withdrawal"
    );
    reopened.close().await?;
    Ok(())
}

#[tokio::test]
async fn sharing_routes_mint_durable_guest_cookies_and_reset_links() -> TestResult {
    use pi_rs_chord::host::Caller;
    use reqwest::{StatusCode, header, redirect::Policy};

    let directory = tempfile::tempdir()?;
    let registry = registry(false)?;
    let owner = serde_json::to_value(Principal::Owner {
        access: crate::api::v1::OwnerAccess::Local,
    })?;
    let context = Context::background();
    let start_sharing = |server: &P2Server, principal| {
        server.host.hold_as(
            Arc::new(lookup("p2.sharing")?),
            JsonValue::from_serde(json!({"session":"one"})),
            Caller {
                principal: Some(principal),
                incarnation: "test".into(),
            },
            &context,
        )
    };
    let server = start(directory.path(), &registry).await?;
    let sharing = start_sharing(&server, owner.clone())?;
    sharing.wait(&context).await?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(Policy::none())
        .build()?;
    let origin = format!("http://{}", server.address);
    sharing
        .call(
            "link",
            JsonValue::from_serde(json!({"mode":"full"})),
            &context,
        )
        .await?;
    let links = sharing.member("links")?.value()?.to_json().to_serde()?;
    let token = links[0]["token"].as_str().ok_or("missing published link")?;
    let link = format!("{origin}/join/{token}");
    let page = client.get(&link).send().await?;
    assert_eq!(page.status(), StatusCode::OK);
    assert!(
        page.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()?
            .contains("script-src 'nonce-")
    );
    assert!(page.text().await?.contains("<h1>One</h1>"));
    let endpoint = format!("{origin}/join");
    let rejected = client
        .post(&endpoint)
        .body(r#"{"token":"x","name":"Reader"}"#)
        .send()
        .await?;
    assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
    let joined = client
        .post(&endpoint)
        .header(header::ORIGIN, &origin)
        .header(header::CONTENT_TYPE, "application/json")
        .body(json!({"token":token,"name":"Reader"}).to_string())
        .send()
        .await?;
    assert_eq!(joined.status(), StatusCode::OK);
    let cookie = joined.headers()[header::SET_COOKIE].to_str()?;
    assert!(cookie.ends_with("; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000"));
    let (cookie, _) = cookie.split_once(';').ok_or("missing guest cookie")?;
    let cookie = cookie.to_owned();
    let (_, secret) = cookie.split_once('=').ok_or("missing cookie value")?;
    let shares = super::super::server::call(
        &server.host,
        Arc::new(lookup(OWNER)?),
        "authenticate",
        json!({"cookie":secret}),
        &context,
    )
    .await?;
    assert_eq!(shares["sessions"]["one"], "full");
    let denied = start_sharing(&server, shares)?;
    assert!(
        matches!(denied.wait(&context).await, Err(Error::Chord(err)) if err.code == "forbidden")
    );
    drop(denied);
    sharing
        .call(
            "share",
            JsonValue::from_serde(json!({
                "mode":"view", "email":" Ann.Example@GoogleMail.com "
            })),
            &context,
        )
        .await?;
    let email = super::super::server::call(
        &server.host,
        Arc::new(lookup(OWNER)?),
        "authenticateEmail",
        json!({"email":"annexample@gmail.com"}),
        &context,
    )
    .await?;
    assert_eq!(email["sessions"]["one"], "view");
    sharing
        .call(
            "remove",
            JsonValue::from_serde(json!({"guestId":email["id"]})),
            &context,
        )
        .await?;
    let removed = super::super::server::call(
        &server.host,
        Arc::new(lookup(OWNER)?),
        "authenticateEmail",
        json!({"email":"annexample@gmail.com"}),
        &context,
    )
    .await?;
    assert!(removed.is_null());
    drop(sharing);
    server.close().await?;

    let reopened = start(directory.path(), &registry).await?;
    let origin = format!("http://{}", reopened.address);
    let port = reopened.address.port();
    let cookie = format!("p2_guest_{port}={secret}");
    let link = format!("{origin}/join/{token}");
    let response = client
        .get(&link)
        .header(header::COOKIE, &cookie)
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/s/one");

    let sharing = start_sharing(&reopened, owner)?;
    sharing.wait(&context).await?;
    sharing
        .call(
            "link",
            JsonValue::from_serde(json!({"mode":"full","reset":true})),
            &context,
        )
        .await?;
    let ended = client
        .get(&link)
        .header(header::COOKIE, &cookie)
        .send()
        .await?;
    assert_eq!(ended.status(), StatusCode::GONE);
    let auth = super::super::server::call(
        &reopened.host,
        Arc::new(lookup(OWNER)?),
        "authenticate",
        json!({"cookie":secret}),
        &context,
    )
    .await?;
    assert!(auth.is_null());
    drop(sharing);
    reopened.close().await?;
    Ok(())
}
