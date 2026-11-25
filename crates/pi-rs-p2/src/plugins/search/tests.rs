//! The search plugin's behaviour: the workers' feed, the index, and the owner's search.

use crate::{
    api::v1::{FacetKind, PluginRegistry, p2_contracts::lookup},
    runtime::principals::Principal,
};
use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::Contract,
    host::{Host, HostOptions, PluginFacet, create_host},
    json::JsonValue,
    link::memory_link_pair,
    remote::{RemoteOptions, RemotePeer, RemoteReference},
    state::Batch,
};
use serde_json::{Value, json};
use std::{future::Future, path::Path, sync::Arc, time::Duration};

/// The contract the index serves.
fn index_contract() -> Result<Arc<Contract>> {
    Ok(Arc::new(lookup("p2.search-index")?))
}

/// The contract the workers feed.
fn feed_contract() -> Result<Arc<Contract>> {
    Ok(Arc::new(lookup("p2.search-feed")?))
}

/// The contract the owner searches.
fn search_contract() -> Result<Arc<Contract>> {
    Ok(Arc::new(lookup("p2.search")?))
}

/// The contract the owner's read markers live in.
fn reads_contract() -> Result<Arc<Contract>> {
    Ok(Arc::new(lookup("p2.session-reads")?))
}

/// A JSON value for a call's arguments.
fn args(value: Value) -> JsonValue {
    JsonValue::from_serde(value)
}

/// A host with the search plugin and the server's own contract, over `directory`.
async fn host(directory: &Path) -> Result<Host> {
    let context = Context::background();
    let host = create_host(HostOptions {
        principal: Some(json!({"kind":"host"})),
        address: Some(args(json!({"kind":"host"}))),
        principal_policy: Some(Principal::POLICY),
        ..Default::default()
    })?;
    let mut registry = PluginRegistry::new();
    crate::plugins::shard_c::register(&mut registry)?;
    let mut facets = registry.facets(FacetKind::Server, &registry.select(None)?);
    facets.push(PluginFacet::new("@p2/core", server_facet(directory)?));
    let ready = host.load(facets, &context).await?;
    if let Some(failure) = ready.failed.first() {
        return Err(Error::Invalid(format!(
            "the search plugin did not load: {}: {}",
            failure.plugin, failure.error
        )));
    }
    Ok(host)
}

/// The server's own contract, with the data directory the index lives in.
fn server_facet(directory: &Path) -> Result<pi_rs_chord::host::Facet> {
    use pi_rs_chord::host::{FacetHost, Members, ObjectSelf, ServeOptions, define_facet};
    let contract = Arc::new(lookup("p2.server")?);
    let info = JsonValue::from_serde(json!({
        "app": "",
        "dataDir": directory.to_string_lossy(),
        "fake": true,
        "allowedHosts": [],
        "port": 0,
    }));
    let served = info.clone();
    Ok(define_facet(move |host: &mut FacetHost| {
        let info = served.clone();
        host.serve(
            contract.clone(),
            move |_params, _principal, object: &mut ObjectSelf| {
                let mut members = Members::new();
                members.state("info", object.writable(info.clone())?)?;
                members.method("revokeOwners", |_args, _ctx| {
                    Box::pin(async { Ok(JsonValue::Null) })
                })?;
                members.method("revokeGuests", |_args, _ctx| {
                    Box::pin(async { Ok(JsonValue::Null) })
                })?;
                Ok(members)
            },
            ServeOptions::default(),
        )?;
        Ok(())
    }))
}

/// A peer that holds contracts as `principal`.
fn peer(host: &Host, principal: Value) -> Result<RemotePeer> {
    let (local, remote) = memory_link_pair();
    host.accept(local, RemoteOptions::new(principal))?;
    RemotePeer::connect(
        remote,
        RemoteOptions::new(json!({"kind":"host"})),
        Batch::new(|_| {}),
    )
}

/// Waits for an object to settle.
async fn settled(future: impl Future<Output = Result<()>>) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .map_err(|error| Error::Invalid(format!("the object never settled: {error}")))?
}

/// The value of a state member.
fn member_value(reference: &RemoteReference, member: &str) -> Result<Value> {
    reference.member(member)?.value()?.to_json().to_serde()
}

/// Waits for a state member to hold `expected`.
async fn holds(reference: &RemoteReference, member: &str, expected: Value) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if member_value(reference, member)? == expected {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(Error::Invalid(format!(
                "the {member} member never settled on {expected}"
            )));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// One indexed message.
fn message(entry_id: i64, role: &str, text: &str) -> Value {
    json!({
        "conversationId": 1,
        "entryId": entry_id,
        "role": role,
        "timestamp": entry_id * 100,
        "text": text,
    })
}

#[tokio::test]
async fn a_workers_messages_are_indexed_and_searched() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let context = Context::background();
    let host = host(directory.path()).await?;
    let worker = peer(
        &host,
        json!({"kind":"worker","session":"active","generation":1}),
    )?;
    let feed = worker.hold(feed_contract()?, args(json!({})), &context)?;
    settled(feed.wait(&context)).await?;
    // The worker feeds its own session: the cursor moves and the messages are indexed.
    feed.call(
        "index",
        args(json!({
            "conversation": 1,
            "messages": [
                message(1, "user", "the draft of tx.diff.ts is ready"),
                message(2, "assistant", "the draft is ready for review"),
            ],
            "through": 2,
        })),
        &context,
    )
    .await?;
    let cursor = feed.call("cursor", args(json!(null)), &context).await?;
    assert_eq!(cursor.to_serde()?, json!(2));
    // The index answers the owner's history search over the conversation.
    let index = host.hold(index_contract()?, args(json!({})), &context)?;
    settled(index.wait(&context)).await?;
    let history = index
        .call(
            "history",
            args(json!({
                "session": "active",
                "query": "draft",
                "conversation": 1,
                "limit": 10,
            })),
            &context,
        )
        .await?;
    let history = history.to_serde()?;
    assert_eq!(history["indexedThrough"], json!(2));
    assert_eq!(history["more"], json!(false));
    assert_eq!(history["hits"].as_array().map(Vec::len), Some(2));
    assert_eq!(history["hits"][0]["role"], json!("assistant"));
    // The message search finds the messages of the sessions it is given.
    let messages = index
        .call(
            "messages",
            args(json!({"query": "draft", "sessions": ["active"]})),
            &context,
        )
        .await?;
    let messages = messages.to_serde()?;
    assert_eq!(messages.as_array().map(Vec::len), Some(2));
    assert_eq!(messages[0]["session"], json!("active"));
    assert!(
        messages[0]["text"]
            .as_str()
            .is_some_and(|text| text.len() <= 300)
    );
    // The mentions tell the rollups which session mentioned the word, and when.
    let mentions = index
        .call(
            "mentions",
            args(json!({"query": "draft", "since": 0})),
            &context,
        )
        .await?;
    assert_eq!(mentions.to_serde()?, json!({"active": 200}));
    // The owner's read markers answer with the answers and what the owner read.
    let owner = peer(&host, json!({"kind":"owner","access":"local"}))?;
    let reads = owner.hold(reads_contract()?, args(json!({})), &context)?;
    settled(reads.wait(&context)).await?;
    holds(&reads, "answers", json!({"active": 2})).await?;
    index
        .call(
            "markRead",
            args(json!({"session": "active", "through": 2})),
            &context,
        )
        .await?;
    holds(&reads, "reads", json!({"active": 2})).await?;
    // The read marker never moves back.
    index
        .call(
            "markRead",
            args(json!({"session": "active", "through": 1})),
            &context,
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(member_value(&reads, "reads")?, json!({"active": 2}));
    worker.close();
    owner.close();
    host.close(&context).await?;
    Ok(())
}

#[tokio::test]
async fn the_index_survives_a_restart() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let context = Context::background();
    {
        let host = host(directory.path()).await?;
        let worker = peer(
            &host,
            json!({"kind":"worker","session":"active","generation":1}),
        )?;
        let feed = worker.hold(feed_contract()?, args(json!({})), &context)?;
        settled(feed.wait(&context)).await?;
        feed.call(
            "index",
            args(json!({
                "conversation": 1,
                "messages": [message(1, "user", "the persisted message")],
                "through": 1,
            })),
            &context,
        )
        .await?;
        worker.close();
        host.close(&context).await?;
    }
    // A fresh host over the same data directory finds the indexed message.
    let host = host(directory.path()).await?;
    let index = host.hold(index_contract()?, args(json!({})), &context)?;
    settled(index.wait(&context)).await?;
    let history = index
        .call(
            "history",
            args(json!({
                "session": "active",
                "query": "persisted",
                "conversation": 1,
                "limit": 10,
            })),
            &context,
        )
        .await?;
    assert_eq!(
        history.to_serde()?["hits"].as_array().map(Vec::len),
        Some(1)
    );
    let cursor = index
        .call("cursor", args(json!({"session": "active"})), &context)
        .await?;
    assert_eq!(cursor.to_serde()?, json!(1));
    host.close(&context).await?;
    Ok(())
}

#[tokio::test]
async fn a_guest_may_not_search_and_a_stranger_may_not_feed() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let context = Context::background();
    let host = host(directory.path()).await?;
    let guest = peer(
        &host,
        json!({
            "kind":"guest",
            "id":"g1",
            "name":"Guest",
            "home":"home",
            "sessions":{"active":"view"},
            "revision":1,
        }),
    )?;
    // The search and the read markers are the owner's alone.
    let denied = guest.hold(search_contract()?, args(json!({})), &context)?;
    assert!(settled(denied.wait(&context)).await.is_err());
    let denied = guest.hold(reads_contract()?, args(json!({})), &context)?;
    assert!(settled(denied.wait(&context)).await.is_err());
    // The feed belongs to a session's own worker.
    let denied = guest.hold(feed_contract()?, args(json!({})), &context)?;
    assert!(settled(denied.wait(&context)).await.is_err());
    guest.close();
    host.close(&context).await?;
    Ok(())
}

#[tokio::test]
async fn a_worker_cannot_index_another_conversation() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let context = Context::background();
    let host = host(directory.path()).await?;
    let worker = peer(
        &host,
        json!({"kind":"worker","session":"active","generation":1}),
    )?;
    let feed = worker.hold(feed_contract()?, args(json!({})), &context)?;
    settled(feed.wait(&context)).await?;
    let refused = feed
        .call(
            "index",
            args(json!({
                "conversation": 2,
                "messages": [message(1, "user", "a message of another conversation")],
                "through": 1,
            })),
            &context,
        )
        .await;
    assert!(refused.is_err());
    worker.close();
    host.close(&context).await?;
    Ok(())
}
