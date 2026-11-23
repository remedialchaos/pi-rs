use std::collections::HashMap;
use std::sync::Arc;

use pi_rs_chord::{
    context::Context,
    contracts::Contract,
    host::{
        Caller, Held, Host, HostOptions, Members, PluginFacet, ServeOptions, create_host,
        define_facet,
    },
    json::JsonValue,
};
use pi_rs_p2::{
    api::v1::{FacetKind, PluginRegistry, p2_contracts::lookup},
    plugins::{
        devices::owner_access::{OwnerAccess, OwnerSessionKind},
        file_index::{
            out_of_scope,
            rank::{FileItem, RankKind, RankOptions, rank_files, score_entry, with_directories},
        },
        logs::{collect_detached_snapshots, register_with_store},
        shard_d,
    },
    runtime::{
        local_owner::local_owner_proof,
        logs::{LogLevel, LogStore},
        principals::Principal,
    },
};
use serde_json::{Value, json};
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

async fn host_with_facets(
    principal: Value,
    address: Value,
    facets: Vec<PluginFacet>,
) -> TestResult<Host> {
    let host = create_host(HostOptions {
        principal: Some(principal),
        address: Some(JsonValue::from_serde(address)),
        principal_policy: Some(Principal::POLICY),
        ..Default::default()
    })?;
    let ctx = Context::background();
    host.load(facets, &ctx).await?;
    Ok(host)
}

async fn get_as(
    host: &Host,
    contract: Arc<Contract>,
    params: JsonValue,
    principal: Option<Value>,
    ctx: &Context,
) -> pi_rs_chord::Result<Held> {
    let held = host.hold_as(
        contract,
        params,
        Caller {
            principal,
            incarnation: host.incarnation().into(),
        },
        ctx,
    )?;
    if let Err(error) = held.wait(ctx).await {
        held.release();
        return Err(error);
    }
    Ok(held)
}

fn is_forbidden<T>(result: &pi_rs_chord::Result<T>) -> bool {
    matches!(result, Err(pi_rs_chord::Error::Chord(err)) if err.code == "forbidden")
}

#[tokio::test]
async fn proxy_marker_serves_without_authenticators() -> TestResult {
    let mut registry = PluginRegistry::new();
    shard_d::register(&mut registry)?;
    let facets = registry.facets(FacetKind::Server, &["proxy-marker".into()]);
    assert_eq!(facets.len(), 1);
    let host = host_with_facets(
        json!({"kind": "server"}),
        json!({"kind": "server", "key": "server", "params": {}}),
        facets,
    )
    .await?;
    let ctx = Context::background();
    let held = host
        .get(
            Arc::new(lookup("p2.proxy-marker")?),
            JsonValue::from_serde(json!({})),
            &ctx,
        )
        .await?;
    assert!(held.member("nonexistent").is_err());
    host.close(&ctx).await?;
    Ok(())
}

#[tokio::test]
async fn drafts_store_claims_atomically_and_enforces_author_access() -> TestResult {
    let mut registry = PluginRegistry::new();
    shard_d::register(&mut registry)?;
    let facets = registry.facets(FacetKind::Worker, &["drafts".into()]);
    let session = "11111111-1111-4111-8111-111111111111";
    let host = host_with_facets(
        json!({"kind": "host"}),
        json!({"kind": "session", "key": session, "params": {"id": session}}),
        facets,
    )
    .await?;
    let ctx = Context::background();

    let store = host
        .get(
            Arc::new(lookup("p2.draft-store")?),
            JsonValue::from_serde(json!({})),
            &ctx,
        )
        .await?;
    store
        .call(
            "add",
            JsonValue::from_serde(json!({
                "conversation": 1,
                "items": [
                    {"author": {"kind": "owner", "id": "owner", "name": "Owner"}, "text": "first owner draft"},
                    {"author": {"kind": "guest", "id": "g1", "name": "Guest"}, "text": "guest draft"},
                    {"author": {"kind": "owner", "id": "owner", "name": "Owner"}, "text": "second owner draft"}
                ]
            })),
            &ctx,
        )
        .await?;

    // View-only guest is forbidden.
    let view_guest = json!({
        "kind": "guest",
        "id": "g1",
        "name": "Guest",
        "home": session,
        "sessions": { (session): "view" },
        "revision": 1
    });
    let denied = get_as(
        &host,
        Arc::new(lookup("p2.drafts")?),
        JsonValue::from_serde(json!({"session": session, "conversation": 1})),
        Some(view_guest),
        &ctx,
    )
    .await;
    assert!(is_forbidden(&denied));

    // Owner sees only owner drafts and claims atomically once.
    let owner = json!({"kind": "owner", "access": "local"});
    let owner_drafts = get_as(
        &host,
        Arc::new(lookup("p2.drafts")?),
        JsonValue::from_serde(json!({"session": session, "conversation": 1})),
        Some(owner),
        &ctx,
    )
    .await?;
    let list = owner_drafts
        .member("drafts")?
        .value()?
        .to_json()
        .to_serde()?;
    assert_eq!(
        list,
        json!([
            {"id": 1, "text": "first owner draft"},
            {"id": 3, "text": "second owner draft"}
        ])
    );

    let claimed = owner_drafts
        .call("claim", JsonValue::from_serde(json!({"ids": [1]})), &ctx)
        .await?
        .to_serde()?;
    assert_eq!(claimed, json!({"texts": ["first owner draft"]}));

    // Claiming the same ID a second time returns empty.
    let again = owner_drafts
        .call("claim", JsonValue::from_serde(json!({"ids": [1]})), &ctx)
        .await?
        .to_serde()?;
    assert_eq!(again, json!({"texts": []}));

    // Full-grant guest sees only their own draft (distinct grant revision).
    let mut full_sessions = serde_json::Map::new();
    full_sessions.insert(session.into(), json!("full"));
    let full_guest = json!({
        "kind": "guest",
        "id": "g1",
        "name": "Guest",
        "home": session,
        "sessions": full_sessions,
        "revision": 2
    });
    let guest_drafts = get_as(
        &host,
        Arc::new(lookup("p2.drafts")?),
        JsonValue::from_serde(json!({"session": session, "conversation": 1})),
        Some(full_guest),
        &ctx,
    )
    .await?;
    assert_eq!(
        guest_drafts
            .member("drafts")?
            .value()?
            .to_json()
            .to_serde()?,
        json!([{"id": 2, "text": "guest draft"}])
    );

    host.close(&ctx).await?;
    Ok(())
}

#[tokio::test]
async fn logs_enforces_owner_only_and_ingests_detached_snapshots() -> TestResult {
    let dir = TempDir::new()?;
    let store = LogStore::default();
    let mut registry = PluginRegistry::new();
    register_with_store(&mut registry, store.clone())?;
    let mut facets = registry.facets(FacetKind::Server, &["logs".into()]);
    let server_contract = Arc::new(lookup("p2.server")?);
    let data_dir = dir.path().to_string_lossy().into_owned();
    facets.push(PluginFacet::new(
        "test-server",
        define_facet(move |host| {
            let data_dir = data_dir.clone();
            host.serve(
                server_contract.clone(),
                move |_, _, object| {
                    let mut members = Members::new();
                    members.state(
                        "info",
                        object.writable(JsonValue::from_serde(json!({
                            "app": "",
                            "dataDir": data_dir,
                            "fake": true,
                            "allowedHosts": [],
                            "port": 8080
                        })))?,
                    )?;
                    members.method("revokeOwners", |_, _| {
                        Box::pin(async { Ok(JsonValue::Null) })
                    })?;
                    members.method("revokeGuests", |_, _| {
                        Box::pin(async { Ok(JsonValue::Null) })
                    })?;
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    ));
    let host = host_with_facets(
        json!({"kind": "server"}),
        json!({"kind": "server", "key": "server", "params": {}}),
        facets,
    )
    .await?;
    let ctx = Context::background();

    // Guest cannot read notices or logs.
    let guest = json!({
        "kind": "guest",
        "id": "g1",
        "name": "Guest",
        "home": "s",
        "sessions": {},
        "revision": 1
    });
    assert!(is_forbidden(
        &get_as(
            &host,
            Arc::new(lookup("p2.log-notices")?),
            JsonValue::from_serde(json!({})),
            Some(guest.clone()),
            &ctx,
        )
        .await
    ));
    assert!(is_forbidden(
        &get_as(
            &host,
            Arc::new(lookup("p2.logs")?),
            JsonValue::from_serde(json!({"source": "server"})),
            Some(guest),
            &ctx,
        )
        .await
    ));

    // Owner can observe notices, dismiss them, and read source lines.
    store.append("server", LogLevel::Event, "started");
    store.notice("server", "TypeError: boom");
    let owner = json!({"kind": "owner", "access": "local"});
    let notices = get_as(
        &host,
        Arc::new(lookup("p2.log-notices")?),
        JsonValue::from_serde(json!({})),
        Some(owner.clone()),
        &ctx,
    )
    .await?;
    let initial = notices.member("notices")?.value()?.to_json().to_serde()?;
    let notice_id = initial[0]["id"].as_u64().ok_or("missing notice id")?;
    notices
        .call(
            "dismiss",
            JsonValue::from_serde(json!({"id": notice_id})),
            &ctx,
        )
        .await?;
    assert!(store.notices().is_empty());

    // Detached snapshot ingestion deduplicates and turns named *Error: lines into notices.
    std::fs::write(
        dir.path().join("terminals.logs.json"),
        serde_json::to_string(&json!({
            "source": "terminals",
            "lines": [
                {"id": 1, "time": 100, "level": "event", "text": "boot"},
                {"id": 2, "time": 101, "level": "error", "text": "RangeError: bad size"}
            ]
        }))?,
    )?;
    let mut fingerprints = HashMap::new();
    let mut seen = HashMap::new();
    collect_detached_snapshots(dir.path(), &store, &mut fingerprints, &mut seen);
    collect_detached_snapshots(dir.path(), &store, &mut fingerprints, &mut seen);
    assert_eq!(store.lines("terminals").len(), 3); // boot + error + notice append
    assert_eq!(store.notices().len(), 1);
    assert_eq!(store.notices()[0].source, "terminals");

    let logs = get_as(
        &host,
        Arc::new(lookup("p2.logs")?),
        JsonValue::from_serde(json!({"source": "terminals"})),
        Some(owner),
        &ctx,
    )
    .await?;
    let lines = logs.member("lines")?.value()?.to_json().to_serde()?;
    assert_eq!(lines.as_array().map(Vec::len), Some(3));

    host.close(&ctx).await?;
    Ok(())
}

#[tokio::test]
async fn file_index_ranking_and_worker_facet_match_upstream() -> TestResult {
    assert!(out_of_scope("/etc/passwd"));
    assert!(out_of_scope("~/secret"));
    assert!(out_of_scope(".."));
    assert!(out_of_scope("../up"));
    assert!(!out_of_scope("src/main.rs"));

    assert!(score_entry("src/app.ts", "app.ts", false) == 100.0);
    assert!(score_entry("src/app-utils.ts", "app", false) == 80.0);
    assert!(score_entry("src/my-app.ts", "app", false) == 50.0);
    assert!(score_entry("app/index.ts", "app", false) == 30.0);
    assert!(score_entry("src/app", "app", true) == 110.0);

    let items = with_directories(&[
        "src/app.ts".into(),
        "src/nested/app-helper.ts".into(),
        "docs/readme.md".into(),
    ]);
    assert!(items.contains(&FileItem {
        path: "src".into(),
        dir: true,
    }));
    assert!(items.contains(&FileItem {
        path: "src/nested".into(),
        dir: true,
    }));

    let ranked = rank_files(
        &items,
        "./app",
        RankOptions {
            limit: 10,
            kind: RankKind::File,
        },
    );
    assert_eq!(ranked[0].path, "src/app.ts");
    assert_eq!(ranked[1].path, "src/nested/app-helper.ts");

    // Live worker facet with mock workspace-backend.
    let mut registry = PluginRegistry::new();
    shard_d::register(&mut registry)?;
    let mut facets = registry.facets(FacetKind::Worker, &["file-index".into()]);
    let wb_contract = Arc::new(lookup("p2.workspace-backend")?);
    facets.push(PluginFacet::new(
        "test-workspace",
        define_facet(move |host| {
            host.serve(
                wb_contract.clone(),
                |_, _, object| {
                    let mut members = Members::new();
                    members.state("changes", object.writable(JsonValue::from_serde(json!(1)))?)?;
                    members.method("query", |args, _| {
                        Box::pin(async move {
                            let args = args.to_serde()?;
                            match args["kind"].as_str() {
                                Some("files") => Ok(JsonValue::from_serde(json!({
                                    "kind": "files",
                                    "paths": ["src/main.rs", "src/lib.rs"],
                                    "truncated": false
                                }))),
                                Some("list") => Ok(JsonValue::from_serde(json!({
                                    "kind": "list",
                                    "entries": [
                                        {"name": "src", "kind": "directory"},
                                        {"name": "Cargo.toml", "kind": "file"}
                                    ],
                                    "truncated": false
                                }))),
                                _ => Err(pi_rs_chord::Error::Invalid("unknown query".into())),
                            }
                        })
                    })?;
                    members.method("read", |_, _| Box::pin(async { Ok(JsonValue::Null) }))?;
                    members.method("write", |_, _| Box::pin(async { Ok(JsonValue::Null) }))?;
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    ));
    let session = "22222222-2222-4222-8222-222222222222";
    let host = host_with_facets(
        json!({"kind": "host"}),
        json!({"kind": "session", "key": session, "params": {"id": session}}),
        facets,
    )
    .await?;
    let ctx = Context::background();

    // View-only guest is denied.
    let viewer = json!({
        "kind": "guest",
        "id": "v1",
        "name": "Viewer",
        "home": session,
        "sessions": { (session): "view" },
        "revision": 1
    });
    assert!(is_forbidden(
        &get_as(
            &host,
            Arc::new(lookup("p2.file-index")?),
            JsonValue::from_serde(json!({"session": session, "conversation": 1})),
            Some(viewer),
            &ctx,
        )
        .await
    ));

    let owner = json!({"kind": "owner", "access": "local"});
    let idx = get_as(
        &host,
        Arc::new(lookup("p2.file-index")?),
        JsonValue::from_serde(json!({"session": session, "conversation": 1})),
        Some(owner),
        &ctx,
    )
    .await?;
    let out_scope = idx
        .call(
            "complete",
            JsonValue::from_serde(json!({"query": "../escape"})),
            &ctx,
        )
        .await?
        .to_serde()?;
    assert_eq!(out_scope, json!({"items": [], "truncated": false}));

    let found = idx
        .call(
            "complete",
            JsonValue::from_serde(json!({"query": "main"})),
            &ctx,
        )
        .await?
        .to_serde()?;
    assert_eq!(found["items"][0]["path"], "src/main.rs");

    let listed = idx
        .call("list", JsonValue::from_serde(json!({"path": ""})), &ctx)
        .await?
        .to_serde()?;
    assert_eq!(
        listed["entries"],
        json!([{"name": "src", "dir": true}, {"name": "Cargo.toml", "dir": false}])
    );

    host.close(&ctx).await?;
    Ok(())
}

#[tokio::test]
async fn devices_owner_access_cookies_challenges_revocation_and_chord_permissions() -> TestResult {
    let dir = TempDir::new()?;
    let access = OwnerAccess::open(dir.path())?;

    // Startup wrote owner-key, owner-cookie-version=signed-v1, and owner-token.txt.
    let key = std::fs::read_to_string(dir.path().join("owner-key"))?
        .trim()
        .to_owned();
    assert_eq!(key.len(), 64);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("owner-cookie-version"))?.trim(),
        "signed-v1"
    );
    let startup_token = std::fs::read_to_string(dir.path().join("owner-token.txt"))?
        .trim()
        .to_owned();
    assert!(access.claim(&startup_token));
    assert!(
        !access.claim(&startup_token),
        "one-time token must not replay"
    );

    // Local challenge-proof uses local_owner_proof and is single-use.
    let challenge = access.issue_challenge();
    let proof = local_owner_proof(&key, 8080, &challenge);
    assert!(access.prove_local(&challenge, &proof, 8080));
    assert!(!access.prove_local(&challenge, &proof, 8080));

    // Device and local cookies validate only on their signed port and stop working on logout/rotate.
    let device_cookie = access.cookie(8080, true);
    let cookie_pair = device_cookie.split(';').next().ok_or("missing cookie")?;
    let parsed = access
        .session_from_cookie_header(cookie_pair, 8080)
        .ok_or("device cookie did not verify")?;
    assert_eq!(parsed.kind, OwnerSessionKind::Device);
    assert!(
        access
            .session_from_cookie_header(cookie_pair, 9090)
            .is_none()
    );

    access.logout(&parsed)?;
    assert!(
        access
            .session_from_cookie_header(cookie_pair, 8080)
            .is_none()
    );

    let local_cookie = access.local_cookie(8080, None);
    let local_pair = local_cookie
        .split(';')
        .next()
        .ok_or("missing local cookie")?;
    assert_eq!(
        access
            .session_from_cookie_header(local_pair, 8080)
            .map(|s| s.kind),
        Some(OwnerSessionKind::Local)
    );
    access.rotate()?;
    assert!(
        access
            .session_from_cookie_header(local_pair, 8080)
            .is_none()
    );

    // Chord p2.devices contract enforces local-only minting and owner-only access.
    let mut registry = PluginRegistry::new();
    shard_d::register(&mut registry)?;
    let mut facets = registry.facets(FacetKind::Server, &["devices".into()]);
    let server_contract = Arc::new(lookup("p2.server")?);
    let data_dir = dir.path().to_string_lossy().into_owned();
    facets.push(PluginFacet::new(
        "test-server",
        define_facet(move |host| {
            let data_dir = data_dir.clone();
            host.serve(
                server_contract.clone(),
                move |_, _, object| {
                    let mut members = Members::new();
                    members.state(
                        "info",
                        object.writable(JsonValue::from_serde(json!({
                            "app": "",
                            "dataDir": data_dir,
                            "fake": true,
                            "allowedHosts": [],
                            "port": 8080
                        })))?,
                    )?;
                    members.method("revokeOwners", |_, _| {
                        Box::pin(async { Ok(JsonValue::Null) })
                    })?;
                    members.method("revokeGuests", |_, _| {
                        Box::pin(async { Ok(JsonValue::Null) })
                    })?;
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    ));
    let host = host_with_facets(
        json!({"kind": "server"}),
        json!({"kind": "server", "key": "server", "params": {}}),
        facets,
    )
    .await?;
    let ctx = Context::background();

    let local_owner = json!({"kind": "owner", "access": "local"});
    let local_devices = get_as(
        &host,
        Arc::new(lookup("p2.devices")?),
        JsonValue::from_serde(json!({})),
        Some(local_owner),
        &ctx,
    )
    .await?;
    assert_eq!(
        local_devices
            .member("canMint")?
            .value()?
            .to_json()
            .to_serde()?,
        json!(true)
    );
    let minted = local_devices
        .call("token", JsonValue::Null, &ctx)
        .await?
        .to_serde()?;
    assert_eq!(minted["token"].as_str().map(str::len), Some(64));

    let remote_owner = json!({"kind": "owner", "access": "device", "device": "d1"});
    let remote_devices = get_as(
        &host,
        Arc::new(lookup("p2.devices")?),
        JsonValue::from_serde(json!({})),
        Some(remote_owner),
        &ctx,
    )
    .await?;
    assert_eq!(
        remote_devices
            .member("canMint")?
            .value()?
            .to_json()
            .to_serde()?,
        json!(false)
    );
    assert!(is_forbidden(
        &remote_devices.call("token", JsonValue::Null, &ctx).await
    ));

    host.close(&ctx).await?;
    Ok(())
}
