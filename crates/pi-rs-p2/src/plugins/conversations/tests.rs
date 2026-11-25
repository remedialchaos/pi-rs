use std::{error::Error as StdError, path::Path, sync::Arc, time::Duration};

use pi_rs_chord::{
    Error,
    context::Context,
    host::{Caller, Held, Host, Members, ServeOptions, define_facet},
    json::JsonValue,
    node::workers::{Control, Frame, WorkerSession, write_frame},
};
use serde_json::{Value, json};
use tokio::io::DuplexStream;

use crate::{
    api::v1::{FacetKind, PluginRegistry, p2_contracts::lookup, session_address},
    hosts::{
        server::{ServerOptions, start_server},
        worker::{SessionWorker, start_session_worker},
    },
};

type TestResult<T = ()> = Result<T, Box<dyn StdError + Send + Sync>>;
const TIMEOUT: Duration = Duration::from_secs(15);
const SESSION: &str = "recipient";

#[tokio::test]
async fn broker_rechecks_worker_generation_and_rejects_archived_senders() -> TestResult {
    let directory = tempfile::tempdir()?;
    let mut options = ServerOptions::new(directory.path().into());
    options.port = 0;
    options.fake = true;
    crate::plugins::shard_a::register(&mut options.registry)?;
    let server = start_server(options).await?;
    let context = Context::background();
    let request = json!({"conversation":1,"requestId":"test-list","query":{"action":"list"}});
    let stale = hold(
        &server.host,
        "p2.conversations",
        json!({}),
        json!({"kind":"worker","session":SESSION,"generation":1}),
        &context,
    )?;
    stale.wait(&context).await?;
    assert!(matches!(
        stale.call("query", request.into(), &context).await,
        Err(Error::Chord(error)) if error.code == "forbidden" && error.message.contains("stale worker")
    ));
    let catalog = server
        .host
        .get(Arc::new(lookup("p2.catalog")?), json!({}).into(), &context)
        .await?;
    let session = uuid::Uuid::new_v4().to_string();
    catalog
        .call(
            "create",
            json!({
                "id":session,"title":"Archived sender","cwd":directory.path(),
                "model":{"provider":"faux","modelId":"faux-1"},"thinkingLevel":"off"
            })
            .into(),
            &context,
        )
        .await?;
    catalog
        .call(
            "update",
            json!({"id":session,"archived":true}).into(),
            &context,
        )
        .await?;
    let broker = server
        .host
        .get(
            Arc::new(lookup("p2.conversation-broker")?),
            json!({}).into(),
            &context,
        )
        .await?;
    assert!(matches!(
        broker.call(
            "query",
            json!({"source":session,"requestId":"test-list","query":{"action":"list"}}).into(),
            &context,
        ).await,
        Err(Error::Chord(error)) if error.code == "forbidden" && error.message.contains("sending session is archived")
    ));
    assert!(server.workers.running().is_empty());
    server.close().await?;
    Ok(())
}

#[tokio::test]
async fn peer_send_is_server_only_and_deduplicated_across_worker_restart() -> TestResult {
    let directory = tempfile::tempdir()?;
    let mut registry = PluginRegistry::new();
    super::register(&mut registry)?;
    let status = Arc::new(lookup("p2.status-report")?);
    registry.register(
        "test-status",
        FacetKind::Worker,
        define_facet(move |host| {
            host.serve(
                status.clone(),
                |_, _, _| {
                    let mut members = Members::new();
                    members.method("push", |_, _| Box::pin(async { Ok(JsonValue::Null) }))?;
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    )?;
    let context = Context::background();
    let (worker, _parent) = start_worker(directory.path(), &registry).await?;
    let params = json!({"session":SESSION});
    let owner = hold(
        &worker.host,
        "p2.peer-conversation",
        params.clone(),
        json!({"kind":"owner","access":"local"}),
        &context,
    )?;
    assert!(matches!(
        owner.wait(&context).await,
        Err(Error::Chord(error)) if error.code == "forbidden"
    ));
    let peer = hold(
        &worker.host,
        "p2.peer-conversation",
        params.clone(),
        json!({"kind":"server"}),
        &context,
    )?;
    let args = json!({
        "source":"sender","title":"Speaker <Example>","requestId":"call-1",
        "message":"persist this attributed request"
    });
    let sent: Value = peer
        .call("send", args.clone().into(), &context)
        .await?
        .to_serde()?;
    let id = sent["submissionId"]
        .as_u64()
        .ok_or("missing submission ID")?;
    let harness = worker.process.harness()?;
    let submission = harness
        .durable
        .submission(id, &context)
        .await?
        .ok_or("missing submission")?;
    let settled = tokio::time::timeout(TIMEOUT, submission.wait(&context)).await??;
    assert_eq!(settled.status, pi_rs_durable::SubmissionStatus::Done);
    tokio::time::timeout(TIMEOUT, harness.durable.wait_for_idle(&context)).await??;
    peer.release();
    drop(harness);
    worker.close().await?;

    let (worker, _parent) = start_worker(directory.path(), &registry).await?;
    let peer = hold(
        &worker.host,
        "p2.peer-conversation",
        params,
        json!({"kind":"server"}),
        &context,
    )?;
    let answer: Value = peer
        .call("answer", JsonValue::Null, &context)
        .await?
        .to_serde()?;
    assert_eq!(
        answer,
        json!({
            "text":"Echo: <sender kind=\"session\" id=\"sender\" title=\"Speaker &lt;Example&gt;\"/>\npersist this attributed request",
            "busy":false,"truncated":false,"limited":false,
        })
    );
    let mut duplicate = args;
    duplicate["message"] = json!("should never be admitted");
    let sent_again: Value = peer
        .call("send", duplicate.into(), &context)
        .await?
        .to_serde()?;
    assert_eq!(sent_again, sent);
    let harness = worker.process.harness()?;
    let root = harness.durable.root(&context).await?;
    let entries = root
        .entries(Default::default(), 20, None, &context)
        .await?
        .items;
    let peer_entries: Vec<_> = entries
        .iter()
        .filter(|entry| entry.kind == "p2.peer-message")
        .collect();
    assert_eq!(peer_entries.len(), 1);
    assert_eq!(
        peer_entries[0].data,
        Some(json!({
            "from":{"id":"sender","title":"Speaker <Example>"},"text":"persist this attributed request"
        }))
    );
    let messages = peer_entries[0]
        .model
        .as_deref()
        .ok_or("missing model input")?;
    assert_eq!(
        messages[0]["content"][0]["text"],
        "<sender kind=\"session\" id=\"sender\" title=\"Speaker &lt;Example&gt;\"/>",
    );
    peer.release();
    drop(harness);
    worker.close().await?;
    Ok(())
}

fn hold(
    host: &Host,
    name: &str,
    params: Value,
    principal: Value,
    context: &Context,
) -> TestResult<Held> {
    Ok(host.hold_as(
        Arc::new(lookup(name)?),
        params.into(),
        Caller {
            principal: Some(principal),
            incarnation: "test".into(),
        },
        context,
    )?)
}

async fn start_worker(
    directory: &Path,
    registry: &PluginRegistry,
) -> TestResult<(SessionWorker, DuplexStream)> {
    let (mut parent, child) = tokio::io::duplex(8192);
    let (read, write) = tokio::io::split(child);
    write_frame(
        &mut parent,
        &Frame::Control(Control::Start {
            address: session_address(SESSION),
            start: json!({
                "app":directory,"dataDir":directory,"generation":1,"fake":true,"telemetry":false,
                "values":{"session":{
                    "id":SESSION,"cwd":directory,"model":{"provider":"faux","modelId":"faux-1"},
                    "thinkingLevel":"off","resume":true,
                }},
            }),
            scripts: Vec::new(),
            lock: directory.join("worker.lock"),
        }),
    )
    .await?;
    let worker = tokio::time::timeout(TIMEOUT, WorkerSession::start(read, write)).await??;
    let worker =
        tokio::time::timeout(TIMEOUT, start_session_worker(worker, registry, None)).await??;
    Ok((worker, parent))
}
