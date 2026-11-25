//! Behavior tests of shard B's built-in plugins against real Chord hosts.

use std::{error::Error as StdError, path::PathBuf, sync::Arc};

use pi_rs_chord::{
    context::Context,
    host::{Facet, Host, HostOptions, Members, PluginFacet, create_host, define_facet},
    json::JsonValue,
    link::memory_link_pair,
    remote::{RemoteOptions, RemotePeer, RemoteReference},
    state::Batch,
};
use pi_rs_p2::api::v1::p2_contracts::lookup;
use pi_rs_p2::plugins::shard_b::ports::server::server_facet;
use serde_json::{Value, json};

/// One fixture set for the contracts the `ports` plugin consumes.
struct Fixtures {
    data_dir: PathBuf,
    port: u16,
    sessions: Value,
    workers: Value,
}

impl Fixtures {
    /// The `p2.server`, `p2.catalog` and `p2.hosts` contracts as the server serves them.
    fn facet(&self) -> pi_rs_chord::Result<Facet> {
        let info = json!({
            "app": "p2",
            "dataDir": self.data_dir.display().to_string(),
            "fake": true,
            "allowedHosts": [],
            "port": self.port,
        });
        let sessions = self.sessions.clone();
        let workers = self.workers.clone();
        let server = Arc::new(lookup("p2.server")?);
        let catalog = Arc::new(lookup("p2.catalog")?);
        let hosts = Arc::new(lookup("p2.hosts")?);
        Ok(define_facet(move |host| {
            let (info, sessions, workers) = (info.clone(), sessions.clone(), workers.clone());
            let server = server.clone();
            host.serve(
                server,
                move |_, _, object| {
                    let mut members = Members::new();
                    members.state(
                        "info",
                        object.writable(JsonValue::from_serde(info.clone()))?,
                    )?;
                    members.method("revokeOwners", move |_, _| {
                        Box::pin(async { Ok(JsonValue::Null) })
                    })?;
                    members.method("revokeGuests", move |_, _| {
                        Box::pin(async { Ok(JsonValue::Null) })
                    })?;
                    Ok(members)
                },
                pi_rs_chord::host::ServeOptions::default(),
            )?;
            let catalog = catalog.clone();
            let sessions_value = sessions.clone();
            host.serve(
                catalog,
                move |_, _, object| {
                    let sessions = sessions_value.clone();
                    let mut members = Members::new();
                    members.state(
                        "sessions",
                        object.writable(JsonValue::from_serde(sessions.clone()))?,
                    )?;
                    let rows = sessions.clone();
                    members.method("get", move |args, _| {
                        let rows = rows.clone();
                        Box::pin(async move {
                            let id = args.to_serde()?["id"].clone();
                            let found = rows
                                .as_array()
                                .and_then(|rows| rows.iter().find(|row| row["id"] == id))
                                .cloned();
                            Ok(JsonValue::from_serde(found.unwrap_or_else(|| {
                                session_row("missing", "Missing", None, true)
                            })))
                        })
                    })?;
                    members.method("create", move |_, _| {
                        Box::pin(async { Ok(JsonValue::Null) })
                    })?;
                    members.method("activity", move |_, _| {
                        Box::pin(async { Ok(JsonValue::Null) })
                    })?;
                    members.method("update", move |_, _| {
                        Box::pin(async { Ok(JsonValue::Null) })
                    })?;
                    Ok(members)
                },
                pi_rs_chord::host::ServeOptions::default(),
            )?;
            let hosts = hosts.clone();
            let workers_value = workers.clone();
            host.serve(
                hosts,
                move |_, _, object| {
                    let workers = workers_value.clone();
                    let mut members = Members::new();
                    members.state(
                        "workers",
                        object.writable(JsonValue::from_serde(workers.clone()))?,
                    )?;
                    members.method("stop", move |_, _| Box::pin(async { Ok(JsonValue::Null) }))?;
                    Ok(members)
                },
                pi_rs_chord::host::ServeOptions::default(),
            )?;
            Ok(())
        }))
    }
}

/// A host with the fixtures and the `ports` plugin, plus a peer connected as `principal`.
/// The peer stays alive in the caller: dropping it closes the link the reference waits on.
async fn ports_host(
    fixtures: &Fixtures,
    principal: Value,
) -> pi_rs_chord::Result<(
    Host,
    RemotePeer,
    RemoteReference,
    Arc<pi_rs_chord::contracts::Contract>,
)> {
    let context = Context::background();
    let host = create_host(HostOptions {
        principal_policy: Some(pi_rs_p2::api::v1::Principal::POLICY),
        address: Some(JsonValue::from_serde(pi_rs_p2::api::v1::server_address())),
        principal: Some(json!({"kind":"server"})),
        ..Default::default()
    })?;
    let loaded = host
        .load(
            vec![
                PluginFacet::new("test-fixtures", fixtures.facet()?),
                PluginFacet::new("ports", server_facet()?),
            ],
            &context,
        )
        .await?;
    if !loaded.failed.is_empty() {
        return Err(pi_rs_chord::Error::Invalid(format!("{:?}", loaded.failed)));
    }
    let contract = Arc::new(lookup("p2.ports")?);
    let (local, remote) = memory_link_pair();
    host.accept(local, RemoteOptions::new(principal))?;
    let peer = RemotePeer::connect(
        remote,
        RemoteOptions::new(json!({"kind":"host"})),
        Batch::new(|_| {}),
    )?;
    let reference = peer.hold(contract.clone(), JsonValue::from_serde(json!({})), &context)?;
    Ok((host, peer, reference, contract))
}

/// One catalog session row, with every field `p2.catalog` v2 requires.
fn session_row(id: &str, title: &str, env: Option<&str>, archived: bool) -> Value {
    json!({
        "id": id,
        "title": title,
        "main": 1,
        "branches": [],
        "createdAt": 1.0,
        "updatedAt": 1.0,
        "cwd": "/tmp",
        "model": {"provider": "faux", "modelId": "faux-1"},
        "pinned": false,
        "thinkingLevel": "off",
        "archivedAt": if archived { json!(1.0) } else { json!(null) },
        "env": env,
    })
}

fn worker_row(generation: u64, running: bool) -> Value {
    json!({"generation": generation, "running": running})
}

/// The owner's Ports sheet: state and methods round-trip, and the list persists.
#[tokio::test]
async fn ports_owner_methods_round_trip_and_persist() -> Result<(), Box<dyn StdError>> {
    let directory = tempfile::tempdir()?;
    let fixtures = Fixtures {
        data_dir: directory.path().to_owned(),
        port: 44323,
        sessions: json!([session_row("s1", "Main", None, false)]),
        workers: json!({"s1": worker_row(1, true)}),
    };
    let context = Context::background();
    let (host, _peer, owner, _) =
        ports_host(&fixtures, json!({"kind":"owner","access":"local"})).await?;
    tokio::time::timeout(std::time::Duration::from_secs(8), owner.wait(&context))
        .await
        .map_err(|_| "owner did not go live")??;
    assert_eq!(owner.variant().as_deref(), Some("owner"));

    // The empty list, with unknown addresses.
    let list = owner.member("forwards")?.value()?.to_json().to_serde()?;
    assert_eq!(list, json!([]));
    assert_eq!(
        owner
            .member("radiusOrigin")?
            .value()?
            .to_json()
            .to_serde()?,
        json!(null)
    );

    // Adding the server's own port is refused; any other port forwards.
    let refused = owner
        .call(
            "add",
            JsonValue::from_serde(json!({"port": 44323, "name": "own"})),
            &context,
        )
        .await
        .err()
        .map(|error| error.to_string());
    assert!(
        refused
            .as_deref()
            .is_some_and(|message| message.contains("Pi's own port")),
        "{refused:?}"
    );
    owner
        .call(
            "add",
            JsonValue::from_serde(json!({"port": 5173, "name": "dev", "public": true})),
            &context,
        )
        .await?;
    let list = owner.member("forwards")?.value()?.to_json().to_serde()?;
    let forward = list
        .as_array()
        .and_then(|rows| rows.first())
        .cloned()
        .ok_or("missing forward")?;
    assert_eq!(forward["port"], json!(5173));
    assert_eq!(forward["name"], json!("dev"));
    assert_eq!(forward["public"], json!(true));
    assert!(forward["id"].as_str().is_some_and(|id| {
        !id.is_empty()
            && id.len() <= 24
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    }));
    // No Radius origin yet: no address, and the session's title is still part of the view.
    assert!(forward.get("url").is_none());

    // Making it private drops the public mark.
    owner
        .call(
            "setPublic",
            JsonValue::from_serde(json!({"id": forward["id"], "public": false})),
            &context,
        )
        .await?;
    let list = owner.member("forwards")?.value()?.to_json().to_serde()?;
    assert_eq!(list[0]["public"].as_bool(), None, "{list:?}");

    // The list persists in forwards.json, tab-indented, and reopens with the same forward.
    let text = tokio::fs::read_to_string(directory.path().join("forwards.json")).await?;
    assert!(text.starts_with('{') && text.contains("\n\t\""));
    let id = forward["id"].as_str().ok_or("id")?.to_owned();
    assert!(text.contains(&format!("\"{id}\"")));
    assert!(text.contains("\"port\": 5173"));
    assert!(text.trim_end().ends_with('}'));

    // Removing the forward empties the list; removing it again fails.
    owner
        .call("remove", JsonValue::from_serde(json!({"id": id})), &context)
        .await?;
    let list = owner.member("forwards")?.value()?.to_json().to_serde()?;
    assert_eq!(list, json!([]));
    let gone = owner
        .call("remove", JsonValue::from_serde(json!({"id": id})), &context)
        .await
        .err()
        .map(|error| error.to_string());
    assert!(gone.is_some_and(|message| message.contains("Unknown forward")));
    peer_close(host).await;
    Ok(())
}

/// The worker's `ports` tool: one forward per port, closed by whoever opened it, and the
/// refusals of stale, archived and remote-environment sessions.
#[tokio::test]
async fn ports_worker_query_opens_and_closes_with_refusals() -> Result<(), Box<dyn StdError>> {
    let directory = tempfile::tempdir()?;
    let fixtures = Fixtures {
        data_dir: directory.path().to_owned(),
        port: 44323,
        sessions: json!([session_row("s1", "Main", None, false)]),
        workers: json!({"s1": worker_row(1, true)}),
    };
    let context = Context::background();
    let (host, _peer, worker, _) = ports_host(
        &fixtures,
        json!({"kind":"worker","session":"s1","generation":1}),
    )
    .await?;
    worker.wait(&context).await?;
    assert_eq!(worker.variant().as_deref(), Some("worker"));

    let reply = worker
        .call(
            "query",
            JsonValue::from_serde(json!({"conversation": 1, "query": {"action": "list"}})),
            &context,
        )
        .await?
        .to_serde()?;
    assert_eq!(reply["forwards"], json!([]));
    assert_eq!(reply["addressesKnown"], json!(false));

    // Opening a port forwards it once; opening it again keeps the address and opener.
    let open = |port: u64| {
        let value =
            json!({"conversation": 1, "query": {"action": "open", "port": port, "name": "dev"}});
        JsonValue::from_serde(value)
    };
    let first = worker
        .call("query", open(5173), &context)
        .await?
        .to_serde()?;
    let id = first["forward"]["id"]
        .as_str()
        .ok_or("forward id")?
        .to_owned();
    assert_eq!(first["forward"]["port"], json!(5173));
    assert_eq!(first["forward"]["session"], json!("s1"));
    assert!(first["forward"].get("url").is_none());
    let second = worker
        .call("query", open(5173), &context)
        .await?
        .to_serde()?;
    assert_eq!(second["forward"]["id"], json!(id));
    assert_eq!(second["forward"]["name"], json!("dev"));
    assert_eq!(second["forward"]["session"], json!("s1"));

    // Closing a forwarded port works; closing it again reports it closed.
    let close = JsonValue::from_serde(
        json!({"conversation": 1, "query": {"action": "close", "port": 5173}}),
    );
    let closed = worker
        .call("query", close.clone(), &context)
        .await?
        .to_serde()?;
    assert_eq!(closed["closed"], json!(true));
    let again = worker.call("query", close, &context).await?.to_serde()?;
    assert_eq!(again["closed"], json!(false));
    peer_close(host).await;

    // A stale worker generation is refused.
    let (host, _peer, stale, _) = ports_host(
        &fixtures,
        json!({"kind":"worker","session":"s1","generation":2}),
    )
    .await?;
    stale.wait(&context).await?;
    let refused = stale
        .call("query", open(5174), &context)
        .await
        .err()
        .map(|error| error.to_string());
    assert!(
        refused
            .as_deref()
            .is_some_and(|message| message.contains("stale worker")),
        "{refused:?}"
    );
    peer_close(host).await;
    Ok(())
}

/// A session on a remote environment cannot forward its host's ports, an archived session is
/// refused, and guests never reach the ports at all.
#[tokio::test]
async fn ports_worker_refuses_archived_remote_and_guests() -> Result<(), Box<dyn StdError>> {
    let context = Context::background();
    let directory = tempfile::tempdir()?;
    let mut fixtures = Fixtures {
        data_dir: directory.path().to_owned(),
        port: 44323,
        sessions: json!([session_row("s1", "Main", Some("gondolin"), false)]),
        workers: json!({"s1": worker_row(1, true)}),
    };
    let (host, _peer, remote_env, _) = ports_host(
        &fixtures,
        json!({"kind":"worker","session":"s1","generation":1}),
    )
    .await?;
    remote_env.wait(&context).await?;
    let open = JsonValue::from_serde(
        json!({"conversation": 1, "query": {"action": "open", "port": 5173}}),
    );
    let refused = remote_env
        .call("query", open.clone(), &context)
        .await
        .err()
        .map(|error| error.to_string());
    assert!(
        refused.as_deref().is_some_and(
            |message| message.contains("ports of remote environments cannot be forwarded yet")
        ),
        "{refused:?}"
    );
    peer_close(host).await;

    fixtures.sessions = json!([session_row("s1", "Main", None, true)]);
    let (host, _peer, archived, _) = ports_host(
        &fixtures,
        json!({"kind":"worker","session":"s1","generation":1}),
    )
    .await?;
    archived.wait(&context).await?;
    let refused = archived
        .call("query", open, &context)
        .await
        .err()
        .map(|error| error.to_string());
    assert!(
        refused
            .as_deref()
            .is_some_and(|message| message.contains("Session is archived")),
        "{refused:?}"
    );
    peer_close(host).await;

    let (host, _peer, guest, _) = ports_host(
        &fixtures,
        json!({"kind":"guest", "id": "g1", "name": "Guest", "home": "s1", "sessions": {}, "revision": 1}),
    )
    .await?;
    let denied = guest
        .wait(&context)
        .await
        .err()
        .map(|error| error.to_string());
    assert!(
        denied
            .as_deref()
            .is_some_and(|message| message.contains("ports are owner-only")),
        "{denied:?}"
    );
    peer_close(host).await;
    Ok(())
}

/// Forwards of archived sessions close when the store opens over their list.
#[tokio::test]
async fn ports_archived_sessions_sweep_at_open() -> Result<(), Box<dyn StdError>> {
    let directory = tempfile::tempdir()?;
    let context = Context::background();
    let fixtures = Fixtures {
        data_dir: directory.path().to_owned(),
        port: 44323,
        sessions: json!([session_row("s1", "Main", None, false)]),
        workers: json!({"s1": worker_row(1, true)}),
    };
    let (host, _peer, worker, _) = ports_host(
        &fixtures,
        json!({"kind":"worker","session":"s1","generation":1}),
    )
    .await?;
    worker.wait(&context).await?;
    worker
        .call(
            "query",
            JsonValue::from_serde(
                json!({"conversation": 1, "query": {"action": "open", "port": 5173}}),
            ),
            &context,
        )
        .await?;
    peer_close(host).await;
    assert!(
        tokio::fs::read_to_string(directory.path().join("forwards.json"))
            .await?
            .contains("\"session\": \"s1\"")
    );

    // The same session, now archived: opening the store sweeps its forward.
    let fixtures = Fixtures {
        data_dir: directory.path().to_owned(),
        port: 44323,
        sessions: json!([session_row("s1", "Main", None, true)]),
        workers: json!({"s1": worker_row(1, false)}),
    };
    let (host, _peer, owner, _) =
        ports_host(&fixtures, json!({"kind":"owner","access":"local"})).await?;
    owner.wait(&context).await?;
    let list = owner.member("forwards")?.value()?.to_json().to_serde()?;
    assert_eq!(list, json!([]));
    peer_close(host).await;
    Ok(())
}

async fn peer_close(host: Host) {
    let _ = host.close(&Context::background()).await;
}

/// Archiving a session immediately closes and deletes only that session's forwards, both when
/// `ForwardStore` is already live and when it is opened on demand.
#[tokio::test]
async fn ports_session_archived_immediately_closes_session_forwards_only()
-> Result<(), Box<dyn StdError>> {
    use pi_rs_chord::host::Caller;
    use pi_rs_p2::hosts::server::{ServerOptions, start_server};

    let directory = tempfile::tempdir()?;
    let s1 = uuid::Uuid::new_v4().to_string();
    let s2 = uuid::Uuid::new_v4().to_string();
    tokio::fs::write(
        directory.path().join("forwards.json"),
        serde_json::to_string_pretty(&json!({
            "fwdowner01": {"port": 5170, "name": "manual"},
            "fwdsess001": {"port": 5173, "name": "s1-web", "session": s1},
            "fwdsess002": {"port": 5174, "name": "s2-web", "session": s2},
        }))?,
    )
    .await?;

    let mut options = ServerOptions::new(directory.path().to_owned());
    options.port = 0;
    options.fake = true;
    options.dev = false;
    pi_rs_p2::plugins::register_builtins(&mut options.registry)?;
    let server = start_server(options).await?;

    let context = Context::background();
    let owner = || Caller {
        principal: Some(json!({"kind": "owner", "access": "local"})),
        incarnation: uuid::Uuid::new_v4().to_string(),
    };
    let sessions = server.host.hold_as(
        Arc::new(lookup("p2.sessions")?),
        JsonValue::from_serde(json!({})),
        owner(),
        &context,
    )?;
    sessions.wait(&context).await?;

    sessions
        .call(
            "create",
            JsonValue::from_serde(json!({
                "id": s1,
                "cwd": directory.path(),
                "title": "Session One",
                "thinkingLevel": "off",
                "model": {"provider": "faux", "modelId": "faux-1"},
            })),
            &context,
        )
        .await?;
    sessions
        .call(
            "create",
            JsonValue::from_serde(json!({
                "id": s2,
                "cwd": directory.path(),
                "title": "Session Two",
                "thinkingLevel": "off",
                "model": {"provider": "faux", "modelId": "faux-1"},
            })),
            &context,
        )
        .await?;

    let ports = server.host.hold_as(
        Arc::new(lookup("p2.ports")?),
        JsonValue::from_serde(json!({})),
        owner(),
        &context,
    )?;
    ports.wait(&context).await?;
    let initial = ports.member("forwards")?.value()?.to_json().to_serde()?;
    let initial_ports: Vec<u64> = initial
        .as_array()
        .ok_or("expected forwards array")?
        .iter()
        .filter_map(|row| row["port"].as_u64())
        .collect();
    assert_eq!(initial_ports, vec![5170, 5173, 5174]);

    // Archiving s1 immediately closes s1's forward while keeping s2 and manual forwards intact.
    sessions
        .call(
            "archive",
            JsonValue::from_serde(json!({"id": s1})),
            &context,
        )
        .await?;

    let after_s1 = ports.member("forwards")?.value()?.to_json().to_serde()?;
    let after_s1_ports: Vec<u64> = after_s1
        .as_array()
        .ok_or("expected forwards array")?
        .iter()
        .filter_map(|row| row["port"].as_u64())
        .collect();
    assert_eq!(after_s1_ports, vec![5170, 5174]);
    let disk_after_s1 = tokio::fs::read_to_string(directory.path().join("forwards.json")).await?;
    assert!(!disk_after_s1.contains(&s1), "{disk_after_s1}");
    assert!(!disk_after_s1.contains("5173"), "{disk_after_s1}");
    assert!(disk_after_s1.contains(&s2), "{disk_after_s1}");
    assert!(disk_after_s1.contains("5170"), "{disk_after_s1}");

    // Even when ForwardStore is no longer held open, archiving s2 still closes s2's forward.
    ports.release();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    sessions
        .call(
            "archive",
            JsonValue::from_serde(json!({"id": s2})),
            &context,
        )
        .await?;
    let disk_after_s2 = tokio::fs::read_to_string(directory.path().join("forwards.json")).await?;
    assert!(!disk_after_s2.contains(&s2), "{disk_after_s2}");
    assert!(!disk_after_s2.contains("5174"), "{disk_after_s2}");
    assert!(disk_after_s2.contains("5170"), "{disk_after_s2}");

    sessions.release();
    server.close().await?;
    Ok(())
}
