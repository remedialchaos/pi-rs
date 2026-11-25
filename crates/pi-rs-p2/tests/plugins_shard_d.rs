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

#[tokio::test]
async fn projects_crud_ordering_assignment_and_owner_only() -> TestResult {
    use pi_rs_p2::plugins::projects::{
        config::{Project, ProjectPlace},
        place::project_at,
    };

    let sample = vec![
        Project {
            name: "root".into(),
            places: vec![ProjectPlace {
                env: None,
                path: "/work".into(),
            }],
        },
        Project {
            name: "sub".into(),
            places: vec![ProjectPlace {
                env: None,
                path: "/work/pi-rs".into(),
            }],
        },
    ];
    assert_eq!(
        project_at(&sample, None, "/work/pi-rs/rust", None).map(|p| p.name.as_str()),
        Some("sub")
    );
    assert_eq!(
        project_at(&sample, None, "/work/other", None).map(|p| p.name.as_str()),
        Some("root")
    );

    let mut registry = PluginRegistry::new();
    shard_d::register(&mut registry)?;
    let facets = registry.facets(FacetKind::Server, &["projects".into()]);
    let host = host_with_facets(
        json!({"kind": "server"}),
        json!({"kind": "server", "key": "server", "params": {}}),
        facets,
    )
    .await?;
    let ctx = Context::background();

    let guest = json!({
        "kind": "guest",
        "id": "g1",
        "name": "Guest",
        "home": "s1",
        "sessions": {},
        "revision": 1
    });
    assert!(is_forbidden(
        &get_as(
            &host,
            Arc::new(lookup("p2.projects")?),
            JsonValue::from_serde(json!({})),
            Some(guest),
            &ctx,
        )
        .await
    ));

    let owner = json!({"kind": "owner", "access": "local"});
    let projects = get_as(
        &host,
        Arc::new(lookup("p2.projects")?),
        JsonValue::from_serde(json!({})),
        Some(owner),
        &ctx,
    )
    .await?;
    projects
        .call(
            "create",
            JsonValue::from_serde(json!({"name": "Alpha", "place": {"env": null, "path": "/a"}})),
            &ctx,
        )
        .await?;
    projects
        .call(
            "create",
            JsonValue::from_serde(json!({"name": "Beta", "place": {"env": null, "path": "/b"}})),
            &ctx,
        )
        .await?;
    projects
        .call(
            "move",
            JsonValue::from_serde(json!({"name": "Beta", "before": "Alpha"})),
            &ctx,
        )
        .await?;
    let ordered = projects.member("projects")?.value()?.to_json().to_serde()?;
    assert_eq!(ordered[0]["name"], "Beta");
    assert_eq!(ordered[1]["name"], "Alpha");

    projects
        .call(
            "assign",
            JsonValue::from_serde(json!({"session": "s1", "project": "Beta"})),
            &ctx,
        )
        .await?;
    projects
        .call(
            "rename",
            JsonValue::from_serde(json!({"name": "Beta", "to": "Gamma"})),
            &ctx,
        )
        .await?;
    let memberships = projects.member("sessions")?.value()?.to_json().to_serde()?;
    assert_eq!(memberships["s1"], "Gamma");

    projects
        .call(
            "archive",
            JsonValue::from_serde(json!({"name": "Gamma"})),
            &ctx,
        )
        .await?;
    let archived = projects.member("archived")?.value()?.to_json().to_serde()?;
    assert_eq!(archived[0]["name"], "Gamma");

    projects
        .call(
            "restore",
            JsonValue::from_serde(json!({"name": "Gamma"})),
            &ctx,
        )
        .await?;
    let live_after = projects.member("projects")?.value()?.to_json().to_serde()?;
    assert_eq!(live_after.as_array().map(Vec::len), Some(2));

    host.close(&ctx).await?;
    Ok(())
}

#[tokio::test]
async fn dev_tools_and_settings_enforce_permissions_and_delegate_services() -> TestResult {
    use pi_rs_p2::{
        api::v1::server::install_service,
        plugins::{
            dev_tools::{PLUGIN_RELOAD_SERVICE, ReloadHandler},
            settings::{SETTINGS_SAVER_SERVICE, SettingsSaver},
        },
    };

    let mut registry = PluginRegistry::new();
    shard_d::register(&mut registry)?;
    let mut facets = registry.facets(FacetKind::Server, &["dev-tools".into(), "settings".into()]);
    let model_cfg = Arc::new(lookup("p2.model-config")?);
    facets.push(PluginFacet::new(
        "test-model-config",
        define_facet(move |host| {
            host.serve(
                model_cfg.clone(),
                |_, _, object| {
                    let mut members = Members::new();
                    members.state(
                        "summary",
                        object.writable(JsonValue::from_serde(json!({
                            "models": [],
                            "defaultModel": null,
                            "errors": []
                        })))?,
                    )?;
                    members.state(
                        "snapshot",
                        object.writable(JsonValue::from_serde(json!({
                            "settings": { "codemode": true, "cacheWarming": "idle" },
                            "errors": [{ "resource": "settings", "message": "bad key" }]
                        })))?,
                    )?;
                    members.method("setDefault", |_, _| Box::pin(async { Ok(JsonValue::Null) }))?;
                    members.method("setEnabledModels", |_, _| {
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

    // dev-tools: owner refused, worker gets default refusal then live handler.
    let owner = json!({"kind": "owner", "access": "local"});
    assert!(is_forbidden(
        &get_as(
            &host,
            Arc::new(lookup("p2.plugin-reload")?),
            JsonValue::from_serde(json!({})),
            Some(owner.clone()),
            &ctx,
        )
        .await
    ));
    let worker = json!({"kind": "worker", "session": "s1", "generation": 1});
    let reload = get_as(
        &host,
        Arc::new(lookup("p2.plugin-reload")?),
        JsonValue::from_serde(json!({})),
        Some(worker),
        &ctx,
    )
    .await?;
    let refused = reload
        .call("reload", JsonValue::from_serde(json!({})), &ctx)
        .await?
        .to_serde()?;
    assert!(refused["refused"].as_str().is_some());

    let handler: Arc<ReloadHandler> = Arc::new(|plugins| {
        Box::pin(async move {
            Ok(JsonValue::from_serde(json!({
                "plugins": plugins.unwrap_or_default().into_iter().map(|p| json!({"plugin": p, "outcome": "applied"})).collect::<Vec<_>>(),
                "logs": []
            })))
        })
    });
    let handler_slot = Arc::new(handler);
    install_service(&host, PLUGIN_RELOAD_SERVICE, &handler_slot)?;
    let ok = reload
        .call(
            "reload",
            JsonValue::from_serde(json!({"plugins": ["drafts"]})),
            &ctx,
        )
        .await?
        .to_serde()?;
    assert_eq!(ok["plugins"][0]["plugin"], "drafts");
    assert_eq!(ok["plugins"][0]["outcome"], "applied");

    // settings: guest refused, owner reads snapshot & delegates update.
    let guest = json!({
        "kind": "guest",
        "id": "g1",
        "name": "Guest",
        "home": "s1",
        "sessions": {},
        "revision": 1
    });
    assert!(is_forbidden(
        &get_as(
            &host,
            Arc::new(lookup("p2.settings")?),
            JsonValue::from_serde(json!({})),
            Some(guest),
            &ctx,
        )
        .await
    ));
    let settings = get_as(
        &host,
        Arc::new(lookup("p2.settings")?),
        JsonValue::from_serde(json!({})),
        Some(owner),
        &ctx,
    )
    .await?;
    assert_eq!(
        settings.member("settings")?.value()?.to_json().to_serde()?["codemode"],
        json!(true)
    );
    assert_eq!(
        settings.member("error")?.value()?.to_json().to_serde()?,
        json!("bad key")
    );
    let captured = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let cap_clone = captured.clone();
    let saver: Arc<SettingsSaver> = Arc::new(move |set, unset| {
        let cap_clone = cap_clone.clone();
        Box::pin(async move {
            let mut guard = cap_clone.lock().unwrap_or_else(|p| p.into_inner());
            guard.extend(set.keys().cloned());
            guard.extend(unset);
            Ok(())
        })
    });
    let saver_slot = Arc::new(saver);
    install_service(&host, SETTINGS_SAVER_SERVICE, &saver_slot)?;
    settings
        .call(
            "update",
            JsonValue::from_serde(json!({"set": {"codemode": false}, "unset": ["ownerName"]})),
            &ctx,
        )
        .await?;
    assert_eq!(
        *captured.lock().unwrap_or_else(|p| p.into_inner()),
        vec!["codemode".to_owned(), "ownerName".to_owned()]
    );

    host.close(&ctx).await?;
    Ok(())
}

#[tokio::test]
async fn status_fences_stale_generation_and_redacts_guests() -> TestResult {
    use pi_rs_p2::plugins::status::{WorkStep, step_of};
    use std::collections::BTreeSet;

    assert_eq!(
        step_of(&["read".into()], &BTreeSet::from(["pi.generation".into()])),
        WorkStep::Tools {
            names: vec!["read".into()]
        }
    );
    assert_eq!(
        step_of(&[], &BTreeSet::from(["pi.compaction".into()])),
        WorkStep::Compacting
    );
    assert_eq!(
        step_of(&[], &BTreeSet::from(["pi.generation".into()])),
        WorkStep::Thinking
    );

    let mut registry = PluginRegistry::new();
    shard_d::register(&mut registry)?;
    let mut facets = registry.facets(FacetKind::Server, &["status".into()]);
    let hosts_contract = Arc::new(lookup("p2.hosts")?);
    facets.push(PluginFacet::new(
        "test-hosts",
        define_facet(move |host| {
            host.serve(
                hosts_contract.clone(),
                |_, _, object| {
                    let mut members = Members::new();
                    members.state(
                        "workers",
                        object.writable(JsonValue::from_serde(json!({
                            "s1": { "running": true, "generation": 2 },
                            "s2": { "running": false, "generation": 1, "error": "secret-crash" }
                        })))?,
                    )?;
                    members.method("stop", |_, _| Box::pin(async { Ok(JsonValue::Null) }))?;
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

    // Stale worker generation (1 != 2) is ignored; current generation (2) updates status.
    let stale_worker = json!({"kind": "worker", "session": "s1", "generation": 1});
    let stale_reporter = get_as(
        &host,
        Arc::new(lookup("p2.session-status")?),
        JsonValue::from_serde(json!({})),
        Some(stale_worker),
        &ctx,
    )
    .await?;
    stale_reporter
        .call(
            "report",
            JsonValue::from_serde(json!({
                "tasks": 99,
                "busy": [1],
                "work": {},
                "failure": { "reason": "stale", "detail": "nope" }
            })),
            &ctx,
        )
        .await?;

    let live_worker = json!({"kind": "worker", "session": "s1", "generation": 2});
    let live_reporter = get_as(
        &host,
        Arc::new(lookup("p2.session-status")?),
        JsonValue::from_serde(json!({})),
        Some(live_worker),
        &ctx,
    )
    .await?;
    live_reporter
        .call(
            "report",
            JsonValue::from_serde(json!({
                "tasks": 3,
                "busy": [1],
                "work": {},
                "failure": { "reason": "rate_limit", "detail": "private-api-key-trace" }
            })),
            &ctx,
        )
        .await?;

    let owner = json!({"kind": "owner", "access": "local"});
    let owner_status = get_as(
        &host,
        Arc::new(lookup("p2.session-status")?),
        JsonValue::from_serde(json!({})),
        Some(owner),
        &ctx,
    )
    .await?;
    let owner_val = owner_status
        .member("statuses")?
        .value()?
        .to_json()
        .to_serde()?;
    assert_eq!(owner_val["s1"]["tasks"], json!(3));
    assert_eq!(
        owner_val["s1"]["failure"]["detail"],
        json!("private-api-key-trace")
    );
    assert_eq!(owner_val["s2"]["error"], json!("secret-crash"));

    let mut guest_sessions = serde_json::Map::new();
    guest_sessions.insert("s1".into(), json!("view"));
    let guest = json!({
        "kind": "guest",
        "id": "g1",
        "name": "Guest",
        "home": "s1",
        "sessions": guest_sessions,
        "revision": 1
    });
    let guest_status = get_as(
        &host,
        Arc::new(lookup("p2.session-status")?),
        JsonValue::from_serde(json!({})),
        Some(guest),
        &ctx,
    )
    .await?;
    let guest_val = guest_status
        .member("statuses")?
        .value()?
        .to_json()
        .to_serde()?;
    assert!(guest_val.get("s2").is_none());
    assert_eq!(guest_val["s1"]["failure"]["reason"], json!("rate_limit"));
    assert!(guest_val["s1"]["failure"].get("detail").is_none());

    host.close(&ctx).await?;
    Ok(())
}

#[tokio::test]
async fn scope_prompt_send_file_files_sync_and_telemetry_behaviors() -> TestResult {
    use pi_rs_chord::node::app::{AppOptions, start_server};
    use pi_rs_p2::{
        api::v1::server::install_service,
        plugins::{
            agent_plugins::remote_hint,
            cache_warming::messages_start_with,
            codemode::build_codemode_extension,
            files::{ByteRange, image_type, parse_byte_range},
            powershell::{offers_powershell, set_powershell},
            quiet_tools::summarize_tool_call,
            scope_prompt::{SCOPE_STAGE_STORE_SERVICE, ScopeStageStore},
            send_file::{WORKER_FILES_SERVICE, preview_text},
            sync::{
                PutBatchResult, RadiusSyncAccess, RadiusSyncError, RadiusSyncRemote,
                is_terminal_sync_error,
            },
            telemetry::{InMemoryTelemetryBackend, TELEMETRY_STORE_SERVICE, TelemetryBackend},
        },
    };
    use pi_rs_telemetry::{InMemoryTelemetryContext, SpanOptions, TelemetryContext};

    // Pure helpers across Shard D1 plugins.
    assert!(remote_hint("/data/plugins").contains("writing-p2-plugins"));
    assert_eq!(image_type("photo.WEBP"), Some("image/webp"));
    assert_eq!(image_type("script.svg"), None);
    assert_eq!(
        parse_byte_range(Some("bytes=0-9"), 100),
        Some(ByteRange::Valid { start: 0, end: 9 })
    );
    assert_eq!(
        parse_byte_range(Some("bytes=-10"), 100),
        Some(ByteRange::Valid { start: 90, end: 99 })
    );
    assert_eq!(
        parse_byte_range(Some("bytes=200-300"), 100),
        Some(ByteRange::Invalid)
    );
    assert_eq!(preview_text(b"hello\x00world", false), None);
    assert_eq!(preview_text(b"hi\xe2\x82", true), Some("hi".into()));
    assert_eq!(preview_text(b"hi\xe2\x82", false), None);

    let mut agent = json!({"tools": ["bash", "read"], "modelTools": {"add": []}});
    assert!(!offers_powershell(Some(&agent)));
    set_powershell(&mut agent, true);
    assert!(offers_powershell(Some(&agent)));
    set_powershell(&mut agent, false);
    assert!(!offers_powershell(Some(&agent)));

    let mut bash_args = serde_json::Map::new();
    bash_args.insert("command".into(), json!("echo hi\necho bye"));
    assert_eq!(summarize_tool_call("bash", &bash_args, &[]), "echo hi …");
    assert!(messages_start_with(&[], &[]));

    assert!(is_terminal_sync_error(&RadiusSyncError::new(
        "denied", 403, None
    )));
    assert!(is_terminal_sync_error(&RadiusSyncError::new(
        "old",
        409,
        Some("protocol_too_old".into())
    )));
    assert!(!is_terminal_sync_error(&RadiusSyncError::new(
        "expired", 401, None
    )));

    // Codemode extension builds cleanly on Wasmtime + QuickJS sandbox.
    let sandbox = pi_rs_codemode::CodemodeSandbox::new(pi_rs_codemode::SandboxOptions::default())?;
    let cm_ext = build_codemode_extension(sandbox);
    assert_eq!(cm_ext.name, "codemode");
    assert_eq!(cm_ext.tools.len(), 1);

    // ScopeStage + SessionFiles + SentFiles on a worker host.
    let files_dir = TempDir::new()?;
    let files_app = start_server(AppOptions::new(
        "shard-d-test",
        files_dir.path().join("data"),
        json!({"kind": "owner"}),
        json!({"kind": "server"}),
    ))
    .await?;
    let files_store = Arc::new(files_app.files.clone());
    let mut registry = PluginRegistry::new();
    shard_d::register(&mut registry)?;
    let facets = registry.facets(
        FacetKind::Worker,
        &["scope-prompt".into(), "files".into(), "send-file".into()],
    );
    let session = "33333333-3333-4333-8333-333333333333";
    let worker_host = host_with_facets(
        json!({"kind": "host"}),
        json!({"kind": "session", "key": session, "params": {"id": session}}),
        facets,
    )
    .await?;
    install_service(&worker_host, WORKER_FILES_SERVICE, &files_store)?;
    let ctx = Context::background();

    // View-only guest cannot stage scope; owner can.
    let mut view_sessions = serde_json::Map::new();
    view_sessions.insert(session.into(), json!("view"));
    let viewer = json!({
        "kind": "guest",
        "id": "v1",
        "name": "Viewer",
        "home": session,
        "sessions": view_sessions,
        "revision": 1
    });
    assert!(is_forbidden(
        &get_as(
            &worker_host,
            Arc::new(lookup("p2.scope-stage")?),
            JsonValue::from_serde(json!({"session": session})),
            Some(viewer.clone()),
            &ctx,
        )
        .await
    ));
    let owner = json!({"kind": "owner", "access": "local"});
    let stage = get_as(
        &worker_host,
        Arc::new(lookup("p2.scope-stage")?),
        JsonValue::from_serde(json!({"session": session})),
        Some(owner.clone()),
        &ctx,
    )
    .await?;
    stage
        .call(
            "stage",
            JsonValue::from_serde(
                json!({"requestId": "req-1", "block": "<scope>x</scope>", "note": "file"}),
            ),
            &ctx,
        )
        .await?;
    let stage_store = pi_rs_p2::api::v1::server::service_of::<ScopeStageStore>(
        &worker_host,
        SCOPE_STAGE_STORE_SERVICE,
    )?;
    assert_eq!(
        stage_store.get("req-1"),
        Some(("<scope>x</scope>".into(), "file".into()))
    );
    assert!(stage_store.drop_staged("req-1"));
    assert_eq!(stage_store.get("req-1"), None);

    // SessionFiles is server-only; upload -> download -> SentFiles.preview works end-to-end.
    assert!(is_forbidden(
        &get_as(
            &worker_host,
            Arc::new(lookup("p2.session-files")?),
            JsonValue::from_serde(json!({"session": session})),
            Some(owner.clone()),
            &ctx,
        )
        .await
    ));
    let server_sf = get_as(
        &worker_host,
        Arc::new(lookup("p2.session-files")?),
        JsonValue::from_serde(json!({"session": session})),
        Some(json!({"kind": "server"})),
        &ctx,
    )
    .await?;
    let begun = server_sf
        .call(
            "begin",
            JsonValue::from_serde(
                json!({"space": "downloads", "name": "report.txt", "source": "server"}),
            ),
            &ctx,
        )
        .await?
        .to_serde()?;
    let upload_id = begun["upload"].as_str().ok_or("missing upload")?;
    server_sf
        .call(
            "append",
            JsonValue::from_serde(
                json!({"upload": upload_id, "offset": 0, "bytes": b"hello world".to_vec()}),
            ),
            &ctx,
        )
        .await?;
    let finished = server_sf
        .call("finish", JsonValue::from_serde(json!(upload_id)), &ctx)
        .await?
        .to_serde()?;
    let file_id = finished["id"].as_str().ok_or("missing id")?;
    let sent = get_as(
        &worker_host,
        Arc::new(lookup("p2.sent-files")?),
        JsonValue::from_serde(json!({"session": session})),
        Some(viewer),
        &ctx,
    )
    .await?;
    let preview = sent
        .call(
            "preview",
            JsonValue::from_serde(json!({"id": file_id})),
            &ctx,
        )
        .await?
        .to_serde()?;
    assert_eq!(preview["text"], json!("hello world"));
    assert_eq!(preview["truncated"], json!(false));
    worker_host.close(&ctx).await?;
    files_app.close().await?;

    // Telemetry server facet: owner reads traces; worker denied if session is shared or locked.
    let mem_telemetry = InMemoryTelemetryContext::new();
    let _ = mem_telemetry
        .start_span(SpanOptions::new("test.op"), |_| async { Ok::<(), ()>(()) })
        .await;
    let mut srv_facets = registry.facets(FacetKind::Server, &["telemetry".into()]);
    let shares_c = Arc::new(lookup("p2.shares")?);
    let locks_c = Arc::new(lookup("p2.session-locks")?);
    srv_facets.push(PluginFacet::new(
        "test-shares-locks",
        define_facet(move |host| {
            host.serve(
                shares_c.clone(),
                |_, _, object| {
                    let mut m = Members::new();
                    m.state(
                        "links",
                        object.writable(JsonValue::from_serde(json!([{
                            "id": "l1",
                            "mode": "view",
                            "session": "shared-s",
                            "token": "t1"
                        }])))?,
                    )?;
                    m.state("emails", object.writable(JsonValue::from_serde(json!([])))?)?;
                    m.state("people", object.writable(JsonValue::from_serde(json!([])))?)?;
                    for name in [
                        "authenticate",
                        "authenticateEmail",
                        "grant",
                        "active",
                        "link",
                        "share",
                        "remove",
                        "join",
                    ] {
                        m.method(name, |_, _| Box::pin(async { Ok(JsonValue::Null) }))?;
                    }
                    Ok(m)
                },
                ServeOptions::default(),
            )?;
            host.serve(
                locks_c.clone(),
                |_, _, object| {
                    let mut m = Members::new();
                    m.state(
                        "locks",
                        object.writable(JsonValue::from_serde(json!({
                            "locked-s": {
                                "capabilities": [],
                                "instructions": false,
                                "mcp": [],
                                "projectMcp": false
                            }
                        })))?,
                    )?;
                    m.method("set", |_, _| Box::pin(async { Ok(JsonValue::Null) }))?;
                    Ok(m)
                },
                ServeOptions::default(),
            )
        }),
    ));
    let tel_host = host_with_facets(
        json!({"kind": "server"}),
        json!({"kind": "server", "key": "server", "params": {}}),
        srv_facets,
    )
    .await?;
    let backend: Arc<dyn TelemetryBackend> = Arc::new(InMemoryTelemetryBackend(mem_telemetry));
    let backend_slot = Arc::new(backend);
    install_service(&tel_host, TELEMETRY_STORE_SERVICE, &backend_slot)?;

    let owner_traces = get_as(
        &tel_host,
        Arc::new(lookup("p2.traces")?),
        JsonValue::from_serde(json!({})),
        Some(owner),
        &ctx,
    )
    .await?;
    let found = owner_traces
        .call("findTraces", JsonValue::from_serde(json!({})), &ctx)
        .await?
        .to_serde()?;
    assert_eq!(found["traces"].as_array().map(Vec::len), Some(1));

    let shared_worker = get_as(
        &tel_host,
        Arc::new(lookup("p2.traces")?),
        JsonValue::from_serde(json!({})),
        Some(json!({"kind": "worker", "session": "shared-s", "generation": 1})),
        &ctx,
    )
    .await?;
    assert!(is_forbidden(
        &shared_worker
            .call("findTraces", JsonValue::from_serde(json!({})), &ctx)
            .await
    ));

    let locked_worker = get_as(
        &tel_host,
        Arc::new(lookup("p2.traces")?),
        JsonValue::from_serde(json!({})),
        Some(json!({"kind": "worker", "session": "locked-s", "generation": 1})),
        &ctx,
    )
    .await?;
    assert!(is_forbidden(
        &locked_worker
            .call("findTraces", JsonValue::from_serde(json!({})), &ctx)
            .await
    ));

    let clean_worker = get_as(
        &tel_host,
        Arc::new(lookup("p2.traces")?),
        JsonValue::from_serde(json!({})),
        Some(json!({"kind": "worker", "session": "private-s", "generation": 1})),
        &ctx,
    )
    .await?;
    let clean_found = clean_worker
        .call("findTraces", JsonValue::from_serde(json!({})), &ctx)
        .await?
        .to_serde()?;
    assert_eq!(clean_found["traces"].as_array().map(Vec::len), Some(1));
    tel_host.close(&ctx).await?;

    // RadiusSyncRemote live HTTP protocol test (201 Created, 409 slot_taken -> Exists, 404 -> None).
    let app = axum::Router::new()
        .route(
            "/v1/servers/srv-1/sync/sessions/sess-1/batches/0",
            axum::routing::put(|| async { axum::http::StatusCode::CREATED }),
        )
        .route(
            "/v1/servers/srv-1/sync/sessions/sess-1/batches/5",
            axum::routing::put(|| async {
                (
                    axum::http::StatusCode::CONFLICT,
                    axum::Json(json!({
                        "error": { "code": "slot_taken", "details": { "to": 10, "device": "dev-a" } }
                    })),
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let srv = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let remote = RadiusSyncRemote::new(Arc::new(move || {
        Box::pin(async move {
            Ok(RadiusSyncAccess {
                gateway: format!("http://{addr}"),
                server_id: "srv-1".into(),
                token: "tok".into(),
            })
        })
    }));
    assert_eq!(
        remote
            .put_batch("sess-1", 0, 5, b"gz".to_vec(), false, &ctx)
            .await?,
        PutBatchResult::Created
    );
    assert_eq!(
        remote
            .put_batch("sess-1", 5, 10, b"gz".to_vec(), false, &ctx)
            .await?,
        PutBatchResult::Exists {
            to: 10,
            device: "dev-a".into()
        }
    );
    assert_eq!(remote.get_batch("sess-1", 99, &ctx).await?, None);
    srv.abort();

    Ok(())
}
