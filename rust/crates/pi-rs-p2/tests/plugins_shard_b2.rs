//! Focused behavior tests for Wave 2 Plugin Shard B2 (`locations`, `models`, `envs`, `workspace`, `terminals`).

use std::{error::Error as StdError, sync::Arc, time::Duration};

use pi_rs_ai::{
    AssistantMessage, StopReason,
    faux::{faux_assistant_message, faux_provider, faux_text, faux_tool_call},
    registry::Models,
};
use pi_rs_chord::{
    context::Context,
    host::{HostOptions, Members, PluginFacet, ServeOptions, create_host, define_facet},
    json::JsonValue,
    link::memory_link_pair,
    remote::{RemoteOptions, RemotePeer},
    state::Batch,
};
use pi_rs_durable::{
    ConversationId, ConversationOwnership, ROOT_CONVERSATION_ID, Session, SubmissionStatus,
    documents::{LatestFork, resolve_address},
    harness::{Extension, Harness, HarnessOptions, HarnessRegistry, configure, create_registry},
    session::SessionOptions,
};
use pi_rs_p2::{
    api::v1::{
        FacetKind, OwnerAccess, PluginRegistry, Principal, p2_contracts::lookup,
        server::install_service, server_address,
    },
    plugins::{
        locations::{
            BoxedFuture, DirectoryListing, ENV_MANAGERS_SERVICE, EnvDirectoryManager, EnvInfo,
            EnvManagers, LocationMoveState, LocationServer, env_revision, location_move_doc,
            server::server_facet as locations_server_facet,
            tool::{LocateConversation, create_location_extension},
        },
        shard_b2::register_builtins,
    },
    runtime::{
        main_conversation::{MainBranch, MainState, set_main_state},
        session_store::open_session_storage,
        session_worker::Location,
    },
};
use serde_json::{Map, Value, json};
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn StdError + Send + Sync>>;
const TIMEOUT: Duration = Duration::from_secs(10);

struct MockEnvManager {
    expected_env: String,
    listing: DirectoryListing,
}

impl EnvDirectoryManager for MockEnvManager {
    fn list_directory<'a>(
        &'a self,
        env: &'a str,
        _path: &'a str,
    ) -> BoxedFuture<'a, DirectoryListing> {
        let ok = env == self.expected_env;
        let listing = self.listing.clone();
        Box::pin(async move {
            if ok {
                Ok(listing)
            } else {
                Err(pi_rs_chord::Error::Invalid(format!("unknown env {env}")))
            }
        })
    }
}

struct MockLocationServer {
    envs: Vec<EnvInfo>,
    local_dir: DirectoryListing,
}

impl LocationServer for MockLocationServer {
    fn envs<'a>(&'a self, _context: &'a Context) -> BoxedFuture<'a, Vec<EnvInfo>> {
        let envs = self.envs.clone();
        Box::pin(async move { Ok(envs) })
    }

    fn directories<'a>(
        &'a self,
        env: Option<&'a str>,
        path: &'a str,
        _context: &'a Context,
    ) -> BoxedFuture<'a, DirectoryListing> {
        let mut listing = self.local_dir.clone();
        if env.is_some() {
            listing.path = path.to_owned();
        }
        Box::pin(async move { Ok(listing) })
    }
}

struct FixedLocator(Location);

impl LocateConversation for FixedLocator {
    fn locate<'a>(
        &'a self,
        _id: ConversationId,
        _session: &'a Session,
        _context: &'a Context,
    ) -> BoxedFuture<'a, Location> {
        let loc = self.0.clone();
        Box::pin(async move { Ok(loc) })
    }
}

fn connect_as(
    host: &pi_rs_chord::host::Host,
    principal: &Principal,
) -> Result<RemotePeer, Box<dyn StdError + Send + Sync>> {
    let (local, remote) = memory_link_pair();
    let pval = serde_json::to_value(principal)?;
    host.accept(local, RemoteOptions::new(pval.clone()))?;
    Ok(RemotePeer::connect(
        remote,
        RemoteOptions::new(pval),
        Batch::new(|_| {}),
    )?)
}

#[test]
fn shard_b2_registers_locations_plugin() -> TestResult {
    let mut registry = PluginRegistry::new();
    register_builtins(&mut registry)?;
    let selected = registry.select(None)?;
    assert!(selected.contains(&"locations".to_owned()));
    assert_eq!(registry.facets(FacetKind::Server, &selected).len(), 1);
    assert_eq!(registry.facets(FacetKind::Worker, &selected).len(), 1);
    Ok(())
}

#[tokio::test]
async fn locations_server_enforces_worker_principal_sandbox_and_lists_envs() -> TestResult {
    let temp = TempDir::new()?;
    let sub_a = temp.path().join("alpha");
    let sub_b = temp.path().join("beta");
    std::fs::create_dir_all(&sub_a)?;
    std::fs::create_dir_all(&sub_b)?;

    let ssh_cfg = json!({ "type": "ssh", "host": "gpu.example", "user": "dev", "port": 2222 });
    let box_cfg = json!({ "type": "gondolin", "host": "gpu" });
    let ssh_rev = env_revision(&ssh_cfg);

    let hosts_contract = Arc::new(lookup("p2.hosts")?);
    let envs_contract = Arc::new(lookup("p2.env-service")?);
    let moves_contract = Arc::new(lookup("p2.session-moves")?);
    let catalog_contract = Arc::new(lookup("p2.catalog")?);
    let agent_locations = Arc::new(lookup("p2.agent-locations")?);

    let relocated = Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
    let relocated_srv = relocated.clone();

    let managers = Arc::new(EnvManagers::new(Arc::new(MockEnvManager {
        expected_env: "gpu".into(),
        listing: DirectoryListing {
            path: "/remote/work".into(),
            parent: Some("/remote".into()),
            directories: vec!["/remote/work/repo".into()],
            truncated: false,
        },
    })));
    let mock_peers = define_facet(move |facet_host| {
        facet_host.serve(
            hosts_contract.clone(),
            |_, _, object| {
                let mut m = Members::new();
                m.state(
                    "workers",
                    object.writable(JsonValue::from_serde(json!({
                        "s-ssh": { "running": true, "generation": 2 },
                        "s-box": { "running": true, "generation": 1 }
                    })))?,
                )?;
                m.method("stop", |_, _| Box::pin(async { Ok(JsonValue::Null) }))?;
                Ok(m)
            },
            ServeOptions::default(),
        )?;

        let ssh_cfg_val = ssh_cfg.clone();
        let box_cfg_val = box_cfg.clone();
        let ssh_rev_val = ssh_rev.clone();
        facet_host.serve(
            envs_contract.clone(),
            move |_, _, object| {
                let mut m = Members::new();
                m.state(
                    "configs",
                    object.writable(JsonValue::from_serde(json!({
                        "gpu": ssh_cfg_val,
                        "box1": box_cfg_val,
                    })))?,
                )?;
                m.state(
                    "tests",
                    object.writable(JsonValue::from_serde(json!({
                        "gpu": {
                            "revision": ssh_rev_val,
                            "test": {
                                "ok": true,
                                "testedAt": 1,
                                "platform": "linux",
                                "arch": "x86_64",
                                "home": "/home/dev",
                                "version": "1",
                                "latencyMs": 10,
                                "watchMode": "native",
                                "warnings": []
                            }
                        },
                        "box1": {
                            "revision": "stale-rev",
                            "test": { "ok": false, "testedAt": 1, "error": "stale" }
                        }
                    })))?,
                )?;
                m.state("failure", object.writable(JsonValue::Null)?)?;
                m.method("test", |_, _| Box::pin(async { Ok(JsonValue::Null) }))?;
                m.method("reload", |_, _| Box::pin(async { Ok(JsonValue::Null) }))?;
                Ok(m)
            },
            ServeOptions::default(),
        )?;

        let relocated_srv = relocated_srv.clone();
        facet_host.serve(
            moves_contract.clone(),
            move |_, _, _| {
                let relocated_srv = relocated_srv.clone();
                let mut m = Members::new();
                m.method("relocate", move |args, _| {
                    let relocated_srv = relocated_srv.clone();
                    Box::pin(async move {
                        relocated_srv.lock().await.push(args.to_serde()?);
                        Ok(JsonValue::from_serde(json!({ "refused": null })))
                    })
                })?;
                Ok(m)
            },
            ServeOptions::default(),
        )?;

        facet_host.serve(
            catalog_contract.clone(),
            |_, _, object| {
                let mut m = Members::new();
                m.state(
                    "sessions",
                    object.writable(JsonValue::from_serde(json!([
                        {
                            "id": "s-ssh",
                            "title": "SSH",
                            "createdAt": 1,
                            "updatedAt": 1,
                            "cwd": "/tmp",
                            "env": "gpu",
                            "model": { "provider": "faux", "modelId": "faux-1" },
                            "pinned": false,
                            "thinkingLevel": "off",
                            "archivedAt": null,
                            "branches": [],
                            "main": 1
                        },
                        {
                            "id": "s-box",
                            "title": "Box",
                            "createdAt": 1,
                            "updatedAt": 1,
                            "cwd": "/tmp",
                            "env": "box1",
                            "model": { "provider": "faux", "modelId": "faux-1" },
                            "pinned": false,
                            "thinkingLevel": "off",
                            "archivedAt": null,
                            "branches": [],
                            "main": 1
                        }
                    ])))?,
                )?;
                m.method("get", |_, _| Box::pin(async { Ok(JsonValue::Null) }))?;
                m.method("create", |_, _| Box::pin(async { Ok(JsonValue::Null) }))?;
                m.method("activity", |_, _| Box::pin(async { Ok(JsonValue::Null) }))?;
                m.method("update", |_, _| Box::pin(async { Ok(JsonValue::Null) }))?;
                Ok(m)
            },
            ServeOptions::default(),
        )?;
        Ok(())
    });

    let ctx = Context::background();
    let host = create_host(HostOptions {
        address: Some(JsonValue::from_serde(server_address())),
        principal: Some(json!({ "kind": "server" })),
        principal_policy: Some(Principal::POLICY),
        ..Default::default()
    })?;
    install_service(&host, ENV_MANAGERS_SERVICE, &managers)?;
    let ready = host
        .load(
            vec![
                PluginFacet::new("peers", mock_peers),
                PluginFacet::new("locations", locations_server_facet()?),
            ],
            &ctx,
        )
        .await?;
    assert!(ready.failed.is_empty(), "{:?}", ready.failed);

    let owner_peer = connect_as(
        &host,
        &Principal::Owner {
            access: OwnerAccess::Local,
        },
    )?;
    let owner_held = owner_peer.hold(
        agent_locations.clone(),
        JsonValue::Object(Default::default()),
        &ctx,
    )?;
    assert!(
        tokio::time::timeout(Duration::from_secs(3), owner_held.wait(&ctx))
            .await
            .map_err(|_| "owner_held.wait timed out")?
            .is_err(),
        "non-worker principal must be rejected"
    );

    let stale_peer = connect_as(
        &host,
        &Principal::Worker {
            session: "s-ssh".into(),
            generation: 1,
        },
    )?;
    let stale_held = stale_peer.hold(
        agent_locations.clone(),
        JsonValue::Object(Default::default()),
        &ctx,
    )?;
    tokio::time::timeout(Duration::from_secs(3), stale_held.wait(&ctx))
        .await
        .map_err(|_| "stale_held.wait timed out")??;
    assert!(
        tokio::time::timeout(
            Duration::from_secs(3),
            stale_held.call("envs", JsonValue::Null, &ctx)
        )
        .await
        .map_err(|_| "stale_held.call timed out")?
        .is_err(),
        "stale worker generation must be rejected"
    );

    let ssh_peer = connect_as(
        &host,
        &Principal::Worker {
            session: "s-ssh".into(),
            generation: 2,
        },
    )?;
    let held = ssh_peer.hold(
        agent_locations.clone(),
        JsonValue::Object(Default::default()),
        &ctx,
    )?;
    tokio::time::timeout(Duration::from_secs(3), held.wait(&ctx))
        .await
        .map_err(|_| "held.wait timed out")??;

    let envs_res = tokio::time::timeout(
        Duration::from_secs(3),
        held.call("envs", JsonValue::Null, &ctx),
    )
    .await
    .map_err(|_| "held.call envs timed out")??
    .to_serde()?;
    let envs = envs_res["envs"].as_array().ok_or("missing envs array")?;
    assert_eq!(envs.len(), 2);
    assert_eq!(envs[0]["name"], "box1");
    assert!(
        envs[0].get("tested").is_none(),
        "stale test revision omitted"
    );
    assert_eq!(envs[1]["name"], "gpu");
    assert_eq!(envs[1]["tested"]["ok"], true);
    assert_eq!(envs[1]["tested"]["platform"], "linux");

    let local_dirs = held
        .call(
            "directories",
            JsonValue::from_serde(json!({ "env": null, "path": temp.path() })),
            &ctx,
        )
        .await?
        .to_serde()?;
    assert_eq!(local_dirs["directories"].as_array().map(Vec::len), Some(2));

    let remote_dirs = held
        .call(
            "directories",
            JsonValue::from_serde(json!({ "env": "gpu", "path": "/remote/work" })),
            &ctx,
        )
        .await?
        .to_serde()?;
    assert_eq!(remote_dirs["path"], "/remote/work");

    let box_peer = connect_as(
        &host,
        &Principal::Worker {
            session: "s-box".into(),
            generation: 1,
        },
    )?;
    let box_held = box_peer.hold(
        agent_locations.clone(),
        JsonValue::Object(Default::default()),
        &ctx,
    )?;
    box_held.wait(&ctx).await?;
    assert!(
        box_held
            .call(
                "move",
                JsonValue::from_serde(json!({ "env": null, "cwd": "/tmp" })),
                &ctx,
            )
            .await
            .is_err(),
        "sandboxed session cannot leave sandbox"
    );

    let move_res = held
        .call(
            "move",
            JsonValue::from_serde(json!({ "env": null, "cwd": temp.path() })),
            &ctx,
        )
        .await?
        .to_serde()?;
    assert_eq!(move_res["refused"], Value::Null);
    assert_eq!(relocated.lock().await.len(), 1);

    drop((
        owner_held, owner_peer, stale_held, stale_peer, held, ssh_peer, box_held, box_peer,
    ));
    tokio::time::timeout(TIMEOUT, host.close(&ctx)).await??;
    Ok(())
}

#[tokio::test]
async fn location_tools_defer_main_move_and_place_subagent() -> TestResult {
    let temp = TempDir::new()?;
    let target_dir = temp.path().join("project");
    std::fs::create_dir_all(&target_dir)?;

    let fake = faux_provider(Default::default());
    let models = Models::default();
    models.set_provider(fake.provider.clone());

    let ext: Extension = create_location_extension(
        Arc::new(MockLocationServer {
            envs: vec![EnvInfo {
                name: "gpu".into(),
                env_type: "ssh".into(),
                host: "gpu.example".into(),
                user: Some("dev".into()),
                port: None,
                tested: None,
            }],
            local_dir: DirectoryListing {
                path: target_dir.to_string_lossy().into_owned(),
                parent: Some(temp.path().to_string_lossy().into_owned()),
                directories: vec![],
                truncated: false,
            },
        }),
        Arc::new(FixedLocator(Location {
            env: None,
            cwd: temp.path().to_string_lossy().into_owned(),
        })),
    );
    let registry = create_registry();
    registry.install(ext)?;

    let open = open_session_storage(temp.path(), "loc-tools").await?;
    let ctx = Context::background();
    let harness = Harness::open_with(
        open.storage.clone(),
        HarnessOptions {
            session: SessionOptions::default(),
            models: models.into(),
            registry: HarnessRegistry::Local(registry).into(),
            ..Default::default()
        },
        &ctx,
    )
    .await?;

    let root = harness.root(&ctx).await?;
    let cfg = Map::from_iter([
        (
            "model".into(),
            json!({ "provider": "faux", "modelId": "faux-1" }),
        ),
        ("thinkingLevel".into(), json!("off")),
        ("cwd".into(), json!(temp.path())),
    ]);
    let cutoff_id = harness
        .session
        .commit(
            async |tx| {
                configure(tx, ROOT_CONVERSATION_ID, &cfg).await?;
                let cutoff = tx
                    .append_entry(
                        ROOT_CONVERSATION_ID,
                        pi_rs_durable::EntryDraft {
                            kind: "pi.user".into(),
                            head: Some(pi_rs_durable::EntryHead::SelfEntry),
                            ..Default::default()
                        },
                    )
                    .await?;
                Ok(cutoff.id)
            },
            &ctx,
        )
        .await?;
    let subagent_id = harness
        .session
        .commit(
            async |tx| {
                let sub = tx
                    .fork_conversation(
                        ROOT_CONVERSATION_ID,
                        cutoff_id,
                        ConversationOwnership::Ownerless,
                    )
                    .await?;
                configure(tx, sub.id, &cfg).await?;
                Ok(sub.id)
            },
            &ctx,
        )
        .await?;

    set_main_state(
        &harness.session,
        &MainState {
            conversation: ROOT_CONVERSATION_ID,
            branches: vec![MainBranch {
                conversation: ROOT_CONVERSATION_ID,
                label: None,
            }],
        },
        &ctx,
    )
    .await?;

    let subagents_doc = Arc::new(pi_rs_durable::documents::DocDefinition {
        kind: "p2.subagents".into(),
        version: 2,
        semantics: pi_rs_durable::documents::DocumentSemantics::LatestConversation {
            fork: LatestFork::Initial,
        },
        initial: pi_rs_durable::documents::InitialDocument::Singleton(Arc::new(|| {
            Ok(serde_json::Map::from_iter([
                ("agents".into(), json!({})),
                ("reporters".into(), json!({})),
            ]))
        })),
        migrate: None,
        checkpoint_when: Arc::new(|_, _, _| true),
    });
    let sub_addr = resolve_address(&subagents_doc, Some(ROOT_CONVERSATION_ID), None)?;
    harness
        .session
        .commit(
            async |tx| {
                let h = tx.doc(&subagents_doc, &sub_addr, None).await?;
                h.set(
                    &[pi_rs_chord::delta::Segment::Key("agents".into())],
                    json!({
                        "worker-1": {
                            "conversationId": subagent_id,
                            "model": "faux/faux-1",
                            "thinkingLevel": "off",
                            "tools": [],
                            "reported": []
                        }
                    })
                    .into(),
                )?;
                Ok(())
            },
            &ctx,
        )
        .await?;

    // Main conversation calls location_move + location_envs
    fake.set_responses([
        AssistantMessage {
            stop_reason: StopReason::ToolUse,
            ..faux_assistant_message(vec![faux_tool_call(
                "location_move",
                serde_json::from_value(json!({ "location": target_dir }))?,
                Some("call-main-move".into()),
            )])
        }
        .into(),
        AssistantMessage {
            stop_reason: StopReason::ToolUse,
            ..faux_assistant_message(vec![faux_tool_call(
                "location_envs",
                Default::default(),
                Some("call-main-envs".into()),
            )])
        }
        .into(),
        faux_assistant_message(vec![faux_text("moved")]).into(),
    ]);
    let sub = root
        .submit(
            serde_json::from_value(json!({ "type": "input", "content": "move main" }))?,
            &ctx,
        )
        .await?;
    assert_eq!(
        tokio::time::timeout(TIMEOUT, sub.wait(&ctx)).await??.status,
        SubmissionStatus::Done
    );

    let move_doc = location_move_doc();
    let move_addr = resolve_address(&move_doc, Some(ROOT_CONVERSATION_ID), None)?;
    let snap = harness
        .session
        .snapshot(&move_doc, &move_addr, &ctx)
        .await?
        .ok_or("missing move doc")?;
    let state: LocationMoveState = serde_json::from_value(Value::Object((*snap).clone()))?;
    assert_eq!(
        state.pending,
        Some(Location {
            env: None,
            cwd: target_dir.to_string_lossy().into_owned()
        })
    );

    let entries = root
        .entries(Default::default(), 32, None, &ctx)
        .await?
        .items;
    assert!(
        entries
            .iter()
            .flat_map(|e| e.model.iter().flatten())
            .any(|m| {
                m["toolCallId"] == "call-main-envs"
                    && m["content"][0]["text"]
                        .as_str()
                        .is_some_and(|t| t.contains("Your session moves to"))
            })
    );

    // Subagent conversation calls location_move -> places subagent immediately
    fake.set_responses([
        AssistantMessage {
            stop_reason: StopReason::ToolUse,
            ..faux_assistant_message(vec![faux_tool_call(
                "location_move",
                serde_json::from_value(json!({ "location": target_dir }))?,
                Some("call-sub-move".into()),
            )])
        }
        .into(),
        faux_assistant_message(vec![faux_text("sub moved")]).into(),
    ]);
    let sub_conv = harness
        .conversation(subagent_id, &ctx)
        .await?
        .ok_or("missing subagent conversation")?;
    let sub_run = sub_conv
        .submit(
            serde_json::from_value(json!({ "type": "input", "content": "move sub" }))?,
            &ctx,
        )
        .await?;
    assert_eq!(
        tokio::time::timeout(TIMEOUT, sub_run.wait(&ctx))
            .await??
            .status,
        SubmissionStatus::Done
    );

    let sub_snap = harness
        .session
        .snapshot(&subagents_doc, &sub_addr, &ctx)
        .await?
        .ok_or("missing subagents doc")?;
    assert_eq!(
        sub_snap["agents"]["worker-1"]["location"]["cwd"],
        target_dir.to_string_lossy().as_ref()
    );

    harness.close(&ctx).await?;
    open.close().await?;
    Ok(())
}
