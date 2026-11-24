//! The message index, the workers' feed into it, and owner search over sessions and messages.
//!
//! The index is derived and rebuildable: the workers feed it their committed messages, and it
//! answers the owner's searches over session titles, working directories and message text.

mod db;
mod query;
#[cfg(test)]
mod tests;

use crate::{
    api::v1::{FacetKind, PluginRegistry, p2_contracts::lookup},
    runtime::principals::Principal,
};
use pi_rs_chord::{
    Error, Result,
    contracts::Contract,
    data::ChordError,
    host::{Ctx, Facet, FacetHost, Held, Members, ObjectSelf, ServeOptions, define_facet},
    json::JsonValue,
    state::Source,
    watch::WatchCall,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use db::{IndexedMessage, SearchDb};
use query::{fts_query, literal_query};

/// The root conversation of a session.
const ROOT_CONVERSATION: i64 = 1;

/// How long a call to another plugin's contract waits for it to answer.
const DEPENDENCY_TIMEOUT: Duration = Duration::from_secs(5);

/// The index's contract: the server's own, host-internal.
fn index_contract() -> Result<Arc<Contract>> {
    Ok(Arc::new(lookup("p2.search-index")?))
}

/// The workers' feed into the index.
fn feed_contract() -> Result<Arc<Contract>> {
    Ok(Arc::new(lookup("p2.search-feed")?))
}

/// The owner's search.
fn search_contract() -> Result<Arc<Contract>> {
    Ok(Arc::new(lookup("p2.search")?))
}

/// The owner's read markers.
fn reads_contract() -> Result<Arc<Contract>> {
    Ok(Arc::new(lookup("p2.session-reads")?))
}

/// The sessions catalog: the sessions and their titles.
fn catalog_contract() -> Result<Arc<Contract>> {
    Ok(Arc::new(lookup("p2.catalog")?))
}

/// The server's own contract: where the index lives.
fn server_contract() -> Result<Arc<Contract>> {
    Ok(Arc::new(lookup("p2.server")?))
}

/// A session's row in the catalog.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogSession {
    id: String,
    title: String,
    cwd: String,
    pinned: bool,
    updated_at: i64,
    archived_at: Option<i64>,
    #[serde(default)]
    main: i64,
    #[serde(default)]
    branches: Vec<CatalogBranch>,
}

/// A branch of a session's root conversation.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogBranch {
    conversation: i64,
}

/// The index's rebuild, as the rebuilds state lists it.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Rebuild {
    session: String,
    phase: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Sidebar order: by directory, pinned first, most recently updated first.
fn sidebar_order(left: &CatalogSession, right: &CatalogSession) -> std::cmp::Ordering {
    left.cwd
        .cmp(&right.cwd)
        .then_with(|| right.pinned.cmp(&left.pinned))
        .then_with(|| right.updated_at.cmp(&left.updated_at))
}

/// The sessions catalog, when the host serves one.
fn catalog_of(ctx: &Ctx) -> Option<Held> {
    let catalog = ctx
        .get(catalog_contract().ok()?, JsonValue::from_serde(json!({})))
        .ok()?;
    // The catalog is served when its rows can be read: without it there are no sessions to search.
    catalog.read("sessions").ok()?;
    Some(catalog)
}

/// A session's row in the catalog.
async fn session_row(ctx: &Ctx, catalog: &Held, session: &str) -> Result<CatalogSession> {
    let row = call_dependency(
        ctx,
        catalog,
        "get",
        JsonValue::from_serde(json!({"id": session})),
    )
    .await?;
    Ok(serde_json::from_value(row.to_serde()?)?)
}

/// The principal of a caller, or the refusal a caller without one gets.
fn principal_of(principal: Option<&Value>, refusal: &str) -> Result<Principal> {
    let value = principal.ok_or_else(|| Error::from(ChordError::new("forbidden", refusal)))?;
    Ok(serde_json::from_value(value.clone())?)
}

/// The data directory of the server, read from its own contract.
async fn data_directory(ctx: &Ctx, server: Arc<Contract>) -> Result<PathBuf> {
    let held = ctx.get(server, JsonValue::from_serde(json!({})))?;
    held.wait(&ctx.context).await?;
    let info: Value = held.read("info")?.to_serde()?;
    let directory = info["dataDir"]
        .as_str()
        .ok_or_else(|| Error::Invalid("The server did not name its data directory".into()))?;
    Ok(PathBuf::from(directory))
}

/// A call to another plugin's contract, bounded: an absent plugin must not hang the caller.
async fn call_dependency(
    ctx: &Ctx,
    held: &Held,
    method: &str,
    args: JsonValue,
) -> Result<JsonValue> {
    match tokio::time::timeout(DEPENDENCY_TIMEOUT, ctx.call(held, method, args)).await {
        Ok(result) => result,
        Err(_) => Err(Error::Invalid(format!(
            "The {method} call timed out; the plugin that serves it is not ready"
        ))),
    }
}

/// The index object: the messages, the cursors and the read markers.
fn index_members(object: &mut ObjectSelf) -> Result<Members> {
    let server = server_contract()?;
    let answers = object.writable(JsonValue::from_serde(json!({})))?;
    let reads = object.writable(JsonValue::from_serde(json!({})))?;
    let rebuilds = object.writable(JsonValue::from_serde(json!([])))?;
    let db = object.open(
        {
            let answers = answers.clone();
            let reads = reads.clone();
            move |ctx: Ctx| {
                let server = server.clone();
                let answers = answers.clone();
                let reads = reads.clone();
                Box::pin(async move {
                    let directory = data_directory(&ctx, server).await?;
                    let db = SearchDb::open(&directory.join("search.sqlite")).await?;
                    answers.set(
                        JsonValue::from_serde(json!(db.answers().await?)),
                        &ctx.context,
                    )?;
                    reads.set(
                        JsonValue::from_serde(json!(db.reads().await?)),
                        &ctx.context,
                    )?;
                    Ok(db)
                })
            }
        },
        |db: Arc<SearchDb>, _reason: String| Box::pin(async move { db.close().await }),
    );
    let mut members = Members::new();
    members.state("answers", answers.clone())?;
    members.state("reads", reads.clone())?;
    members.state("rebuilds", rebuilds.clone())?;
    members.method("cursor", {
        let db = db.clone();
        move |args: JsonValue, _ctx: Ctx| {
            let db = db.clone();
            Box::pin(async move {
                let args = args.to_serde()?;
                let session = session_of(&args)?;
                let conversation = conversation_of(&args);
                let through = db.value()?.cursor(&session, conversation).await?;
                Ok(JsonValue::from_serde(json!(through)))
            })
        }
    })?;
    members.method("index", {
        let db = db.clone();
        let answers = answers.clone();
        move |args: JsonValue, ctx: Ctx| {
            let db = db.clone();
            let answers = answers.clone();
            Box::pin(async move {
                let args = args.to_serde()?;
                let session = session_of(&args)?;
                let conversation = conversation_of(&args);
                let messages: Vec<IndexedMessage> =
                    serde_json::from_value(args["messages"].clone())?;
                let through = args["through"].as_i64().unwrap_or_default();
                let index = db.value()?;
                index
                    .index(&session, conversation, &messages, through)
                    .await?;
                // The session's activity follows its messages, and its newest root answer is what the owner has not read yet.
                let timestamp = messages
                    .iter()
                    .map(|message| message.timestamp)
                    .max()
                    .unwrap_or_else(|| args["activityAt"].as_i64().unwrap_or_default());
                let answer = messages
                    .iter()
                    .filter(|message| message.role == "assistant")
                    .map(|message| message.entry_id)
                    .max()
                    .unwrap_or_default();
                let mut current: BTreeMap<String, i64> =
                    serde_json::from_value(answers.value()?.to_json().to_serde()?)?;
                // Unread answers are the root conversation's and its branches': a branch left while running
                // answers in the background.
                let mut root = conversation == ROOT_CONVERSATION;
                let catalog = catalog_of(&ctx);
                if !root
                    && let Some(catalog) = &catalog
                    && let Ok(row) = session_row(&ctx, catalog, &session).await
                {
                    root = conversation == row.main
                        || row
                            .branches
                            .iter()
                            .any(|branch| branch.conversation == conversation);
                }
                if root && answer > current.get(&session).copied().unwrap_or_default() {
                    current.insert(session.clone(), answer);
                    answers.set(JsonValue::from_serde(json!(current)), &ctx.context)?;
                }
                if let Some(catalog) = catalog {
                    let mut activity = json!({"id": session, "timestamp": timestamp});
                    if let Some(first) = index.first_user_text(&session).await? {
                        activity["firstUserText"] = json!(first);
                    }
                    call_dependency(&ctx, &catalog, "activity", JsonValue::from_serde(activity))
                        .await?;
                }
                Ok(JsonValue::Null)
            })
        }
    })?;
    members.method("reset", {
        let db = db.clone();
        move |args: JsonValue, _ctx: Ctx| {
            let db = db.clone();
            Box::pin(async move {
                let args = args.to_serde()?;
                let session = session_of(&args)?;
                db.value()?.reset(&session).await?;
                Ok(JsonValue::Null)
            })
        }
    })?;
    members.method("markRead", {
        let db = db.clone();
        let reads = reads.clone();
        move |args: JsonValue, ctx: Ctx| {
            let db = db.clone();
            let reads = reads.clone();
            Box::pin(async move {
                let args = args.to_serde()?;
                let session = session_of(&args)?;
                let through = args["through"].as_i64().unwrap_or_default();
                let index = db.value()?;
                let current: BTreeMap<String, i64> =
                    serde_json::from_value(reads.value()?.to_json().to_serde()?)?;
                if through <= current.get(&session).copied().unwrap_or_default() {
                    return Ok(JsonValue::Null);
                }
                index.mark_read(&session, through).await?;
                let mut current = current;
                current.insert(session, through);
                reads.set(JsonValue::from_serde(json!(current)), &ctx.context)?;
                Ok(JsonValue::Null)
            })
        }
    })?;
    members.method("history", {
        let db = db.clone();
        move |args: JsonValue, _ctx: Ctx| {
            let db = db.clone();
            Box::pin(async move {
                let args = args.to_serde()?;
                let session = session_of(&args)?;
                let conversation = args["conversation"].as_i64().unwrap_or(1);
                let query = args["query"].as_str().unwrap_or_default();
                let before = args["before"].as_i64();
                let limit = args["limit"].as_i64().unwrap_or(20).clamp(1, 20);
                let index = db.value()?;
                let indexed_through = index.cursor(&session, conversation).await?;
                let Some(expression) = fts_query(query) else {
                    return Ok(JsonValue::from_serde(json!({
                        "hits": [],
                        "more": false,
                        "indexedThrough": indexed_through,
                    })));
                };
                let found = index
                    .conversation_messages(&session, conversation, &expression, before, limit)
                    .await?;
                let hits: Vec<Value> = found
                    .hits
                    .iter()
                    .map(|hit| {
                        json!({
                            "entryId": hit.entry_id,
                            "role": hit.role,
                            "snippet": hit.snippet,
                        })
                    })
                    .collect();
                Ok(JsonValue::from_serde(json!({
                    "hits": hits,
                    "more": found.more,
                    "indexedThrough": indexed_through,
                })))
            })
        }
    })?;
    members.method("messages", {
        let db = db.clone();
        move |args: JsonValue, _ctx: Ctx| {
            let db = db.clone();
            Box::pin(async move {
                let args = args.to_serde()?;
                let query = args["query"].as_str().unwrap_or_default();
                let sessions: Vec<String> = args["sessions"]
                    .as_array()
                    .map(|rows| {
                        rows.iter()
                            .filter_map(|row| row.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                let Some(expression) = literal_query(query) else {
                    return Ok(JsonValue::Array(Vec::<JsonValue>::new()));
                };
                let hits = db.value()?.messages(&expression, &sessions).await?;
                Ok(JsonValue::from_serde(Value::Array(
                    hits.iter()
                        .map(|hit| {
                            json!({
                                "session": hit.session,
                                "conversationId": hit.conversation_id,
                                "entryId": hit.entry_id,
                                "role": hit.role,
                                "text": hit.text,
                            })
                        })
                        .collect::<Vec<Value>>(),
                )))
            })
        }
    })?;
    members.method("mentions", {
        let db = db.clone();
        move |args: JsonValue, _ctx: Ctx| {
            let db = db.clone();
            Box::pin(async move {
                let args = args.to_serde()?;
                let query = args["query"].as_str().unwrap_or_default();
                let since = args["since"].as_i64().unwrap_or_default();
                let Some(expression) = fts_query(query) else {
                    return Ok(JsonValue::from_serde(json!({})));
                };
                let mentions = db.value()?.mentions(&expression, since).await?;
                Ok(JsonValue::from_serde(json!(mentions)))
            })
        }
    })?;
    members.method("rebuild", {
        let db = db.clone();
        let rebuilds = rebuilds.clone();
        move |args: JsonValue, ctx: Ctx| {
            let db = db.clone();
            let rebuilds = rebuilds.clone();
            Box::pin(async move {
                let args = args.to_serde()?;
                let session = session_of(&args)?;
                // A rebuild starts from the catalog: an unknown session is refused at once.
                let catalog = ctx.get(catalog_contract()?, JsonValue::from_serde(json!({})))?;
                if let Err(error) = call_dependency(
                    &ctx,
                    &catalog,
                    "get",
                    JsonValue::from_serde(json!({"id": session})),
                )
                .await
                {
                    let failure = Rebuild {
                        session: session.clone(),
                        phase: "failed".into(),
                        error: Some(error.to_string()),
                    };
                    let mut list: Vec<Rebuild> =
                        serde_json::from_value(rebuilds.value()?.to_json().to_serde()?)?;
                    list.retain(|entry| entry.session != session);
                    list.push(failure);
                    rebuilds.set(JsonValue::from_serde(json!(list)), &ctx.context)?;
                    return Err(error);
                }
                db.value()?.reset(&session).await?;
                Ok(JsonValue::Null)
            })
        }
    })?;
    Ok(members)
}

/// The session a call names.
fn session_of(args: &Value) -> Result<String> {
    args["session"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::Invalid("The call names no session".into()))
}

/// The conversation a call names (1: the root conversation).
fn conversation_of(args: &Value) -> i64 {
    args["conversation"].as_i64().unwrap_or(1)
}

/// The workers' feed into the index: only a session's own worker feeds it.
fn feed_members(object: &mut ObjectSelf, principal: Option<&Value>) -> Result<Members> {
    let Principal::Worker { session, .. } =
        principal_of(principal, "The search feed is for session workers")?
    else {
        return Err(ChordError::new("forbidden", "The search feed is for session workers").into());
    };
    let index = object.get(index_contract()?, JsonValue::from_serde(json!({})))?;
    let mut members = Members::variant("worker");
    members.method("cursor", {
        let index = index.clone();
        let session = session.clone();
        move |args: JsonValue, ctx: Ctx| {
            let index = index.clone();
            let session = session.clone();
            Box::pin(async move {
                let args = args.to_serde()?;
                let conversation = args
                    .get("conversation")
                    .and_then(|value| value.as_i64())
                    .unwrap_or(1);
                ctx.call(
                    &index,
                    "cursor",
                    JsonValue::from_serde(
                        json!({"session": session, "conversation": conversation}),
                    ),
                )
                .await
            })
        }
    })?;
    members.method("index", {
        let index = index.clone();
        let session = session.clone();
        move |args: JsonValue, ctx: Ctx| {
            let index = index.clone();
            let session = session.clone();
            Box::pin(async move {
                let args = args.to_serde()?;
                let conversation = args
                    .get("conversation")
                    .and_then(|value| value.as_i64())
                    .unwrap_or(1);
                let messages = args["messages"].as_array().cloned().unwrap_or_default();
                if messages.iter().any(|message| {
                    message["conversationId"].as_i64().unwrap_or(conversation) != conversation
                }) {
                    return Err(ChordError::new(
                        "forbidden",
                        "Index messages must belong to the batch conversation",
                    )
                    .into());
                }
                ctx.call(
                    &index,
                    "index",
                    JsonValue::from_serde(json!({
                        "session": session,
                        "conversation": conversation,
                        "messages": messages,
                        "through": args["through"].clone(),
                    })),
                )
                .await
            })
        }
    })?;
    members.method("history", {
        let index = index.clone();
        let session = session.clone();
        move |args: JsonValue, ctx: Ctx| {
            let index = index.clone();
            let session = session.clone();
            Box::pin(async move {
                let args = args.to_serde()?;
                ctx.call(
                    &index,
                    "history",
                    JsonValue::from_serde(json!({
                        "session": session,
                        "query": args["query"].clone(),
                        "conversation": args["conversation"].clone(),
                        "before": args["before"].clone(),
                        "limit": args["limit"].clone(),
                    })),
                )
                .await
            })
        }
    })?;
    Ok(members)
}

/// The owner's search: over session titles, working directories and message text.
fn search_members(object: &mut ObjectSelf, principal: Option<&Value>) -> Result<Members> {
    let Principal::Owner { .. } = principal_of(principal, "Search is owner-only")? else {
        return Err(ChordError::new("forbidden", "Search is owner-only").into());
    };
    let index = object.get(index_contract()?, JsonValue::from_serde(json!({})))?;
    let mut members = Members::variant("owner");
    members.method("search", {
        let index = index.clone();
        move |args: JsonValue, ctx: Ctx| {
            let index = index.clone();
            Box::pin(async move {
                let args = args.to_serde()?;
                let query = args["query"].as_str().unwrap_or_default();
                let archived = args["archived"].as_bool().unwrap_or_default();
                let listed: Option<Vec<String>> = args["sessions"].as_array().map(|rows| {
                    rows.iter()
                        .filter_map(|row| row.as_str().map(str::to_owned))
                        .collect()
                });
                // The catalog lists the sessions a search covers: the sidebar's order, and their titles.
                let catalog = ctx.get(catalog_contract()?, JsonValue::from_serde(json!({})))?;
                let sessions: Vec<CatalogSession> =
                    serde_json::from_value(catalog.read("sessions")?.to_serde()?).map_err(
                        |error| {
                            Error::Invalid(format!("The sessions catalog is unreadable: {error}"))
                        },
                    )?;
                let scope: Vec<&CatalogSession> = sessions
                    .iter()
                    .filter(|session| session.archived_at.is_some() == archived)
                    .filter(|session| {
                        listed
                            .as_ref()
                            .is_none_or(|listed| listed.contains(&session.id))
                    })
                    .collect();
                let needle = query.to_lowercase();
                let mut matched: Vec<&CatalogSession> = scope
                    .iter()
                    .copied()
                    .filter(|session| {
                        format!("{}\n{}", session.title, session.cwd)
                            .to_lowercase()
                            .contains(&needle)
                    })
                    .collect();
                matched.sort_by(|left, right| sidebar_order(left, right));
                let titles: BTreeMap<&str, &str> = scope
                    .iter()
                    .map(|session| (session.id.as_str(), session.title.as_str()))
                    .collect();
                let messages = if query.trim().is_empty() {
                    Vec::new()
                } else {
                    let hits = ctx
                        .call(
                            &index,
                            "messages",
                            JsonValue::from_serde(json!({
                                "query": query,
                                "sessions": scope.iter().map(|session| session.id.clone()).collect::<Vec<_>>(),
                            })),
                        )
                        .await?;
                    serde_json::from_value(hits.to_serde()?)?
                };
                let messages: Vec<Value> = messages
                    .into_iter()
                    .map(|mut hit: Value| {
                        let session = hit["session"].as_str().unwrap_or_default();
                        hit["title"] = json!(titles.get(session).copied().unwrap_or_default());
                        hit
                    })
                    .collect();
                Ok(JsonValue::from_serde(json!({
                    "sessions": matched.iter().map(|session| session.id.clone()).collect::<Vec<_>>(),
                    "messages": messages,
                })))
            })
        }
    })?;
    members.method("rebuildIndex", {
        let index = index.clone();
        move |args: JsonValue, ctx: Ctx| {
            let index = index.clone();
            Box::pin(async move {
                let args = args.to_serde()?;
                let session = session_of(&args)?;
                ctx.call(
                    &index,
                    "rebuild",
                    JsonValue::from_serde(json!({"session": session})),
                )
                .await
            })
        }
    })?;
    Ok(members)
}

/// The owner's read markers: the same answers and reads the index keeps.
fn reads_members(object: &mut ObjectSelf, principal: Option<&Value>) -> Result<Members> {
    let Principal::Owner { .. } = principal_of(principal, "Read markers are owner-only")? else {
        return Err(ChordError::new("forbidden", "Read markers are owner-only").into());
    };
    let index = object.get(index_contract()?, JsonValue::from_serde(json!({})))?;
    let answers = mirror(object, index.clone(), "answers")?;
    let reads = mirror(object, index.clone(), "reads")?;
    let mut members = Members::variant("owner");
    members.state("answers", answers)?;
    members.state("reads", reads)?;
    members.method("markRead", {
        let index = index.clone();
        move |args: JsonValue, ctx: Ctx| {
            let index = index.clone();
            Box::pin(async move { ctx.call(&index, "markRead", args).await })
        }
    })?;
    Ok(members)
}

/// A state member that follows the index's own.
fn mirror(object: &mut ObjectSelf, index: Held, member: &'static str) -> Result<Source> {
    object.follow(move |target: Source, ctx: Ctx| {
        let index = index.clone();
        Box::pin(async move {
            index.wait(&ctx.context).await?;
            let source = index.member(member)?;
            target.set(source.value()?.to_json(), &ctx.context)?;
            let changed = target.clone();
            let ended = target.clone();
            let subscription = source.subscribe(
                move |call, context| match call {
                    WatchCall::Value { value, .. } => changed.set(value, context),
                    WatchCall::Failed { error } => changed.fail(error, context),
                },
                move |_| ended.detach("ended"),
            );
            Ok(Box::new(move || drop(subscription)) as Box<dyn FnOnce() + Send>)
        })
    })
}

/// Registers the search plugin into `registry`.
pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    registry.register("search", FacetKind::Server, facet()?)?;
    Ok(())
}

/// The plugin's facet: the index, the feed, the owner's search and the read markers.
fn facet() -> Result<Facet> {
    Ok(define_facet(|host: &mut FacetHost| {
        let index = index_contract()?;
        let feed = feed_contract()?;
        let search = search_contract()?;
        let reads = reads_contract()?;
        host.serve(
            index.clone(),
            |_params, _principal, object| index_members(object),
            ServeOptions::default(),
        )?;
        host.serve(
            feed.clone(),
            |_params, principal, object| feed_members(object, principal),
            ServeOptions::default(),
        )?;
        host.serve(
            search.clone(),
            |_params, principal, object| search_members(object, principal),
            ServeOptions::default(),
        )?;
        host.serve(
            reads.clone(),
            |_params, principal, object| reads_members(object, principal),
            ServeOptions::default(),
        )?;
        Ok(())
    }))
}
