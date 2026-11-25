//! `logs` built-in plugin: owner-only process logs & notices (`p2.logs`, `p2.log-notices`),
//! `POST /client-error` browser error intake, and detached-process log snapshot ingestion.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::body::Body;
use http::{Response, StatusCode};
use pi_rs_chord::{
    Error, Result,
    data::ChordError,
    host::{Members, ServeOptions, define_facet},
    json::JsonValue,
    node::http::{HttpContribution, HttpResponse, RouteOptions},
};
use serde::Deserialize;
use serde_json::json;

use crate::{
    api::v1::{FacetKind, PluginRegistry, Principal, p2_contracts::lookup},
    runtime::{
        http::same_origin,
        logs::{LogLevel, LogStore, process_logs},
    },
};

const CLIENT_REPORT_BYTES: usize = 16 * 1024;
const CLIENT_REPORTS_PER_MINUTE: usize = 60;
const MAX_SNAPSHOT_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SNAPSHOT_LINES: usize = 1000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientReport {
    where_: String,
    message: String,
    stack: String,
    path: String,
    version: String,
}

impl ClientReport {
    fn parse(raw: &[u8]) -> Option<Self> {
        let mut value: serde_json::Value = serde_json::from_slice(raw).ok()?;
        let obj = value.as_object_mut()?;
        let where_ = obj.remove("where")?;
        obj.insert("where_".into(), where_);
        let report: Self = serde_json::from_value(value).ok()?;
        if report.where_.chars().count() > 40
            || report.message.chars().count() > 2000
            || report.stack.chars().count() > 4000
            || report.path.chars().count() > 400
            || report.version.chars().count() > 80
        {
            return None;
        }
        Some(report)
    }
}

#[derive(Deserialize)]
struct SnapshotFile {
    source: String,
    lines: Vec<SnapshotLine>,
}

#[derive(Deserialize)]
struct SnapshotLine {
    id: u64,
    time: u64,
    level: SnapshotLevel,
    text: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum SnapshotLevel {
    Stdout,
    Stderr,
    Event,
    Error,
}

impl From<SnapshotLevel> for LogLevel {
    fn from(value: SnapshotLevel) -> Self {
        match value {
            SnapshotLevel::Stdout => Self::Stdout,
            SnapshotLevel::Stderr => Self::Stderr,
            SnapshotLevel::Event => Self::Event,
            SnapshotLevel::Error => Self::Error,
        }
    }
}

struct RateWindow {
    start: Instant,
    count: usize,
}

impl RateWindow {
    fn allow(&mut self) -> bool {
        let now = Instant::now();
        if now.duration_since(self.start) > Duration::from_secs(60) {
            self.start = now;
            self.count = 0;
        }
        self.count += 1;
        self.count <= CLIENT_REPORTS_PER_MINUTE
    }
}

fn is_named_error_line(text: &str) -> bool {
    let Some(colon) = text.find(':') else {
        return false;
    };
    let prefix = &text[..colon];
    prefix.ends_with("Error")
        && prefix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Ingest new lines from `<data_dir>/{terminals,dev}.logs.json` into `store`.
pub fn collect_detached_snapshots(
    directory: &std::path::Path,
    store: &LogStore,
    fingerprints: &mut HashMap<String, String>,
    seen: &mut HashMap<String, HashSet<String>>,
) {
    for source in ["terminals", "dev"] {
        let path = directory.join(format!("{source}.logs.json"));
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.len() > MAX_SNAPSHOT_BYTES {
            continue;
        }
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        let fingerprint = format!("{modified}:{}", meta.len());
        if fingerprints.get(source).map(String::as_str) == Some(fingerprint.as_str()) {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(snapshot) = serde_json::from_str::<SnapshotFile>(&raw) else {
            continue;
        };
        if snapshot.source != source || snapshot.lines.len() > MAX_SNAPSHOT_LINES {
            continue;
        }
        let prior = seen.get(source);
        let mut next = HashSet::with_capacity(snapshot.lines.len());
        for line in snapshot.lines {
            let key = format!("{}:{}", line.id, line.time);
            next.insert(key.clone());
            if !prior.is_some_and(|keys| keys.contains(&key)) {
                let level: LogLevel = line.level.into();
                store.append_at(source, level, &line.text, line.time);
                if matches!(level, LogLevel::Error) && is_named_error_line(&line.text) {
                    store.notice(source, &line.text);
                }
            }
        }
        seen.insert(source.into(), next);
        fingerprints.insert(source.into(), fingerprint);
    }
}

fn empty_status(status: StatusCode) -> HttpResponse {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    response
}

fn require_owner(principal: Option<&serde_json::Value>, message: &str) -> Result<Principal> {
    let parsed = principal
        .and_then(|value| Principal::deserialize(value).ok())
        .ok_or_else(|| ChordError::new("forbidden", message))?;
    if !parsed.is_owner() {
        return Err(ChordError::new("forbidden", message).into());
    }
    Ok(parsed)
}

/// Register the `logs` server facet backed by `store`.
pub fn register_with_store(registry: &mut PluginRegistry, store: LogStore) -> Result<()> {
    let notices_contract = Arc::new(lookup("p2.log-notices")?);
    let logs_contract = Arc::new(lookup("p2.logs")?);
    let server_contract = Arc::new(lookup("p2.server")?);

    registry.register(
        "logs",
        FacetKind::Server,
        define_facet(move |host| {
            let rate = Arc::new(Mutex::new(RateWindow {
                start: Instant::now() - Duration::from_secs(61),
                count: 0,
            }));
            let route_store = store.clone();
            let mut http = HttpContribution::new("logs");
            http.route(
                "POST /client-error",
                RouteOptions::public(move |request| {
                    let rate = rate.clone();
                    let route_store = route_store.clone();
                    Box::pin(async move {
                        let principal = request
                            .principal
                            .as_ref()
                            .and_then(|value| Principal::deserialize(value).ok());
                        let is_owner = principal.as_ref().is_some_and(Principal::is_owner);
                        let is_guest = matches!(principal, Some(Principal::Guest { .. }));
                        if !same_origin(&request.headers, &request.origin)
                            || (!is_owner && !is_guest)
                        {
                            return Ok(empty_status(StatusCode::FORBIDDEN));
                        }
                        let body = match request.body(CLIENT_REPORT_BYTES).await {
                            Ok(bytes) => bytes,
                            Err(_) => return Ok(empty_status(StatusCode::BAD_REQUEST)),
                        };
                        let Some(report) = ClientReport::parse(&body) else {
                            return Ok(empty_status(StatusCode::BAD_REQUEST));
                        };
                        let allowed = rate
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner())
                            .allow();
                        if allowed {
                            let who = if is_owner { "owner" } else { "guest" };
                            let stack = if report.stack.is_empty() {
                                String::new()
                            } else {
                                format!("\n{}", report.stack)
                            };
                            let text = format!(
                                "Page error ({}, {who}, {}, version {}): {}{stack}",
                                report.where_, report.path, report.version, report.message
                            );
                            if is_owner {
                                route_store.notice("web", &text);
                            } else {
                                route_store.append("web", LogLevel::Error, &text);
                            }
                        }
                        Ok(empty_status(StatusCode::NO_CONTENT))
                    })
                }),
            )
            .map_err(|err| Error::Invalid(err.to_string()))?;
            host.contribute_http(http)?;

            let notice_store = store.clone();
            host.serve(
                notices_contract.clone(),
                move |_, principal, object| {
                    require_owner(principal, "Notices are only available to owners")?;
                    let sub_store = notice_store.clone();
                    let notices = object.follow(move |target, ctx| {
                        Box::pin(async move {
                            let initial =
                                JsonValue::from_serde(serde_json::to_value(sub_store.notices())?);
                            target.set(initial, &ctx.context)?;
                            let target_cb = target.clone();
                            let reader = sub_store.clone();
                            let sub_id = sub_store.subscribe(Arc::new(move |_| {
                                if let Ok(value) = serde_json::to_value(reader.notices()) {
                                    let _ = target_cb.set(
                                        JsonValue::from_serde(value),
                                        &pi_rs_chord::context::Context::background(),
                                    );
                                }
                            }));
                            let unsub_store = sub_store.clone();
                            Ok(Box::new(move || unsub_store.unsubscribe(sub_id))
                                as Box<dyn FnOnce() + Send>)
                        })
                    })?;

                    let mut members = Members::variant("owner");
                    members.state("notices", notices)?;
                    let dismiss_store = notice_store.clone();
                    members.method("dismiss", move |args, _| {
                        let dismiss_store = dismiss_store.clone();
                        Box::pin(async move {
                            let id = args.to_serde()?["id"]
                                .as_u64()
                                .ok_or_else(|| Error::Invalid("missing notice id".into()))?;
                            dismiss_store.dismiss_notice(id);
                            Ok(JsonValue::Null)
                        })
                    })?;
                    Ok(members)
                },
                ServeOptions::default(),
            )?;

            let logs_store = store.clone();
            let server_ref = server_contract.clone();
            host.serve(
                logs_contract.clone(),
                move |params, principal, object| {
                    require_owner(principal, "Logs are only available to owners")?;
                    let source = params.to_serde()?["source"]
                        .as_str()
                        .ok_or_else(|| Error::Invalid("missing source param".into()))?
                        .to_owned();

                    // Also ensure snapshot collection can run against data_dir when p2.server is present.
                    if let Ok(server) =
                        object.get(server_ref.clone(), JsonValue::from_serde(json!({})))
                    {
                        let poll_store = logs_store.clone();
                        object.on_live(move |ctx| {
                            let server = server.clone();
                            let poll_store = poll_store.clone();
                            Box::pin(async move {
                                let info = match server.member("info").and_then(|s| s.value()) {
                                    Ok(v) => v.to_json().to_serde().ok(),
                                    Err(_) => None,
                                };
                                let Some(dir) = info
                                    .as_ref()
                                    .and_then(|v| v.get("dataDir"))
                                    .and_then(|v| v.as_str())
                                    .map(PathBuf::from)
                                else {
                                    return Ok(());
                                };
                                poll_store.add_directory(&dir);
                                let mut fingerprints = HashMap::new();
                                let mut seen = HashMap::new();
                                let mut interval =
                                    tokio::time::interval(Duration::from_millis(300));
                                loop {
                                    if ctx.context.await_with(interval.tick()).await.is_err() {
                                        break;
                                    }
                                    collect_detached_snapshots(
                                        &dir,
                                        &poll_store,
                                        &mut fingerprints,
                                        &mut seen,
                                    );
                                }
                                Ok(())
                            })
                        });
                    }

                    let sources_store = logs_store.clone();
                    let sources = object.follow(move |target, ctx| {
                        Box::pin(async move {
                            let initial = JsonValue::from_serde(serde_json::to_value(
                                sources_store.sources(),
                            )?);
                            target.set(initial, &ctx.context)?;
                            let target_cb = target.clone();
                            let reader = sources_store.clone();
                            let sub_id = sources_store.subscribe(Arc::new(move |_| {
                                if let Ok(value) = serde_json::to_value(reader.sources()) {
                                    let _ = target_cb.set(
                                        JsonValue::from_serde(value),
                                        &pi_rs_chord::context::Context::background(),
                                    );
                                }
                            }));
                            let unsub_store = sources_store.clone();
                            Ok(Box::new(move || unsub_store.unsubscribe(sub_id))
                                as Box<dyn FnOnce() + Send>)
                        })
                    })?;

                    let lines_store = logs_store.clone();
                    let wanted_source = source.clone();
                    let lines = object.follow(move |target, ctx| {
                        Box::pin(async move {
                            let initial = JsonValue::from_serde(serde_json::to_value(
                                lines_store.lines(&wanted_source),
                            )?);
                            target.set(initial, &ctx.context)?;
                            let target_cb = target.clone();
                            let reader = lines_store.clone();
                            let source_name = wanted_source.clone();
                            let sub_id = lines_store.subscribe(Arc::new(move |changed_source| {
                                let matches = changed_source == source_name
                                    || !reader
                                        .sources()
                                        .iter()
                                        .any(|entry| entry.id == source_name);
                                if matches
                                    && let Ok(value) =
                                        serde_json::to_value(reader.lines(&source_name))
                                {
                                    let _ = target_cb.set(
                                        JsonValue::from_serde(value),
                                        &pi_rs_chord::context::Context::background(),
                                    );
                                }
                            }));
                            let unsub_store = lines_store.clone();
                            Ok(Box::new(move || unsub_store.unsubscribe(sub_id))
                                as Box<dyn FnOnce() + Send>)
                        })
                    })?;

                    let mut members = Members::variant("owner");
                    members.state("sources", sources)?;
                    members.state("lines", lines)?;
                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    )
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    register_with_store(registry, process_logs().clone())
}
