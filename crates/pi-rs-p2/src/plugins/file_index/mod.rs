//! `file-index` built-in plugin: `@` completion ranking, directory listing, and worker facets
//! (`p2.file-index-cache` and `p2.file-index`).

pub mod listing;
pub mod rank;

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use pi_rs_chord::{
    Error, Result,
    data::ChordError,
    host::{Members, ServeOptions, define_facet},
    json::JsonValue,
};
use serde::Deserialize;
use serde_json::json;

use crate::api::v1::{FacetKind, PluginRegistry, Principal, p2_contracts::lookup};

use rank::{FileItem, RankKind, RankOptions, rank_files, with_directories};

const REBUILD_MS: Duration = Duration::from_millis(1000);

/// Prefixes outside the session directory: completion is cwd-relative (section 8.2).
pub fn out_of_scope(query: &str) -> bool {
    query.starts_with('/') || query.starts_with('~') || query == ".." || query.starts_with("../")
}

/// Valid relative path within a workspace (rejects absolute, `~`, `..`, NUL, and control chars).
fn is_valid_relative_path(path: &str) -> bool {
    if path.starts_with('/')
        || path.starts_with('\\')
        || path.starts_with('~')
        || path.bytes().any(|b| b < 0x20 || b == 0x7f)
    {
        return false;
    }
    for segment in path.split(['/', '\\']) {
        if segment.is_empty() || segment == "." || segment == ".." {
            return false;
        }
    }
    true
}

#[derive(Deserialize)]
struct CompleteArgs {
    query: String,
    limit: Option<usize>,
    kind: Option<String>,
}

#[derive(Deserialize)]
struct ListArgs {
    path: String,
}

#[derive(Clone)]
struct CachedIndex {
    changes: u64,
    items: Vec<FileItem>,
    truncated: bool,
    at: Instant,
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    let cache_contract = Arc::new(lookup("p2.file-index-cache")?);
    let index_contract = Arc::new(lookup("p2.file-index")?);
    let workspace_backend_contract = Arc::new(lookup("p2.workspace-backend")?);

    registry.register(
        "file-index",
        FacetKind::Worker,
        define_facet(move |host| {
            let cache_for_remote = cache_contract.clone();
            let backend_contract = workspace_backend_contract.clone();
            host.serve(
                cache_contract.clone(),
                move |params, _, object| {
                    let conversation = params.to_serde()?["conversation"]
                        .as_u64()
                        .ok_or_else(|| Error::Invalid("missing conversation param".into()))?;
                    let backend = object.get(
                        backend_contract.clone(),
                        JsonValue::from_serde(json!({ "conversation": conversation })),
                    )?;
                    let changes_source = object.follow_source(backend.member("changes")?)?;
                    let cached = Arc::new(Mutex::new(None::<CachedIndex>));

                    let mut members = Members::new();
                    members.state("changes", changes_source)?;

                    let complete_backend = backend.clone();
                    let complete_cache = cached.clone();
                    members.method("complete", move |args, ctx| {
                        let complete_backend = complete_backend.clone();
                        let complete_cache = complete_cache.clone();
                        Box::pin(async move {
                            let args: CompleteArgs = serde_json::from_value(args.to_serde()?)?;
                            if out_of_scope(&args.query) {
                                return Ok(JsonValue::from_serde(json!({
                                    "items": [],
                                    "truncated": false,
                                })));
                            }
                            let changes = complete_backend
                                .member("changes")
                                .and_then(|s| s.value())
                                .ok()
                                .and_then(|v| v.to_json().to_serde().ok())
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            let hit = {
                                let guard = complete_cache
                                    .lock()
                                    .unwrap_or_else(|poison| poison.into_inner());
                                guard.as_ref().and_then(|entry| {
                                    (entry.changes == changes || entry.at.elapsed() < REBUILD_MS)
                                        .then(|| entry.clone())
                                })
                            };
                            let built = if let Some(entry) = hit {
                                entry
                            } else {
                                let result = complete_backend
                                    .call(
                                        "query",
                                        JsonValue::from_serde(json!({ "kind": "files" })),
                                        &ctx.context,
                                    )
                                    .await?
                                    .to_serde()?;
                                if result["kind"].as_str() != Some("files") {
                                    return Err(Error::Invalid(
                                        "Unexpected workspace result".into(),
                                    ));
                                }
                                let paths: Vec<String> =
                                    serde_json::from_value(result["paths"].clone())?;
                                let truncated = result["truncated"].as_bool().unwrap_or(false);
                                let entry = CachedIndex {
                                    changes,
                                    items: with_directories(&paths),
                                    truncated,
                                    at: Instant::now(),
                                };
                                *complete_cache
                                    .lock()
                                    .unwrap_or_else(|poison| poison.into_inner()) =
                                    Some(entry.clone());
                                entry
                            };
                            let ranked = rank_files(
                                &built.items,
                                &args.query,
                                RankOptions {
                                    limit: args.limit.unwrap_or(20),
                                    kind: RankKind::parse(args.kind.as_deref()),
                                },
                            );
                            Ok(JsonValue::from_serde(json!({
                                "items": ranked,
                                "truncated": built.truncated,
                            })))
                        })
                    })?;

                    let list_backend = backend.clone();
                    members.method("list", move |args, ctx| {
                        let list_backend = list_backend.clone();
                        Box::pin(async move {
                            let args: ListArgs = serde_json::from_value(args.to_serde()?)?;
                            let relative = args.path.trim_end_matches('/');
                            if !relative.is_empty() && !is_valid_relative_path(relative) {
                                return Err(Error::Invalid("Invalid path".into()));
                            }
                            let result = list_backend
                                .call(
                                    "query",
                                    JsonValue::from_serde(json!({
                                        "kind": "list",
                                        "path": relative,
                                    })),
                                    &ctx.context,
                                )
                                .await?
                                .to_serde()?;
                            if result["kind"].as_str() != Some("list") {
                                return Err(Error::Invalid("Unexpected workspace result".into()));
                            }
                            let entries: Vec<serde_json::Value> = result["entries"]
                                .as_array()
                                .cloned()
                                .unwrap_or_default()
                                .into_iter()
                                .map(|entry| {
                                    json!({
                                        "name": entry["name"],
                                        "dir": entry["kind"].as_str() == Some("directory"),
                                    })
                                })
                                .collect();
                            Ok(JsonValue::from_serde(json!({
                                "entries": entries,
                                "truncated": result["truncated"].as_bool().unwrap_or(false),
                            })))
                        })
                    })?;

                    Ok(members)
                },
                ServeOptions {
                    linger: Duration::from_secs(60),
                    ..ServeOptions::default()
                },
            )?;

            host.serve(
                index_contract.clone(),
                move |params, principal, object| {
                    let params = params.to_serde()?;
                    let session = params["session"]
                        .as_str()
                        .ok_or_else(|| Error::Invalid("missing session param".into()))?;
                    let conversation = params["conversation"]
                        .as_u64()
                        .ok_or_else(|| Error::Invalid("missing conversation param".into()))?;
                    let parsed = principal
                        .and_then(|value| Principal::deserialize(value).ok())
                        .ok_or_else(|| ChordError::new("forbidden", "participants only"))?;
                    if !parsed.can_send(session) {
                        return Err(ChordError::new("forbidden", "participants only").into());
                    }
                    let cache = object.get(
                        cache_for_remote.clone(),
                        JsonValue::from_serde(json!({ "conversation": conversation })),
                    )?;
                    let changes = object.follow_source(cache.member("changes")?)?;
                    let mut members = Members::variant("reader");
                    members.state("changes", changes)?;
                    let complete_cache = cache.clone();
                    members.method("complete", move |args, ctx| {
                        let complete_cache = complete_cache.clone();
                        Box::pin(async move {
                            complete_cache.call("complete", args, &ctx.context).await
                        })
                    })?;
                    let list_cache = cache.clone();
                    members.method("list", move |args, ctx| {
                        let list_cache = list_cache.clone();
                        Box::pin(async move { list_cache.call("list", args, &ctx.context).await })
                    })?;
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    )
}
