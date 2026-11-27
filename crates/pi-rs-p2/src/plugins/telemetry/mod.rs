//! `telemetry` built-in plugin: owner & unshared/unlocked worker read access to the server's
//! telemetry store (`p2.traces`) and worker-side `telemetry_*` code-mode tools.

use std::{path::Path, sync::Arc};

use indexmap::IndexMap;
use pi_rs_ai::{InputContent, Tool};
use pi_rs_chord::{
    Error, Result,
    context::Context,
    contracts::{Contract, HostKind, Member, read_params},
    data::ChordError,
    host::{Facet, Members, ServeOptions, define_facet},
    json::JsonValue,
    sync::SyncFuture,
    telemetry::{IngestFacet, IngestOptions, IngestRegistry, SqliteTelemetry, worker_batch_schema},
};
use pi_rs_durable::{
    harness::{Extension, ToolRegistration, define_extension},
    tools::ToolResult,
};
use pi_rs_telemetry::{
    AttributeValue, FallibleTelemetryStore, MetricQuery, SpanAttributes, SpanStatus, StoredSpan,
    StoredTrace, TelemetryStore, TraceQuery, memory::InMemoryTelemetryContext, render_trace,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::api::v1::{
    FacetKind, PluginRegistry, Principal,
    p2_contracts::lookup,
    server::service_of,
    worker::{
        durable::{LockedTool, ToolAccess},
        worker_of,
    },
};

pub const TELEMETRY_OFF: &str = "Telemetry is off on this server.";
pub const MAX_FILTERS: usize = 8;
pub const MAX_SPANS: usize = 2000;
pub const MAX_METRICS: usize = 1000;
const DEFAULT_TRACES: usize = 20;
const MAX_HOSTS: usize = 8;

/// Native service key under which the server registers its live telemetry query backend.
pub const TELEMETRY_STORE_SERVICE: &str = "p2.telemetry-store";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceSummary {
    pub trace_id: String,
    pub name: String,
    pub started_ms: f64,
    pub duration_ms: f64,
    pub spans: usize,
    pub errors: usize,
    pub hosts: Vec<String>,
}

/// Summarizes a stored trace: earliest span name, start, total duration, span & error counts, hosts.
pub fn summarize_trace(trace: &StoredTrace) -> TraceSummary {
    let mut first: Option<&StoredSpan> = None;
    let mut end: f64 = 0.0;
    let mut errors = 0usize;
    let mut hosts: Vec<String> = Vec::new();
    for span in &trace.spans {
        if first.is_none_or(|f| span.started_ms < f.started_ms) {
            first = Some(span);
        }
        if span.ended_ms > end {
            end = span.ended_ms;
        }
        if matches!(span.status, SpanStatus::Error { .. }) {
            errors += 1;
        }
        if let Some(AttributeValue::String(host)) = span.attributes.get("chord.host")
            && hosts.len() < MAX_HOSTS
            && !hosts.contains(host)
        {
            hosts.push(host.clone());
        }
    }
    let started_ms = first.map_or(0.0, |s| s.started_ms);
    let duration_ms = first.map_or(0.0, |_| end - started_ms);
    TraceSummary {
        trace_id: trace.trace_id.clone(),
        name: first.map_or_else(String::new, |s| s.name.clone()),
        started_ms,
        duration_ms,
        spans: trace.spans.len(),
        errors,
        hosts,
    }
}

/// Resolves whether server telemetry is enabled from explicit option, `P2_TELEMETRY`,
/// or `<data_dir>/settings.json` (`telemetry: bool`, defaulting to `true`).
pub fn telemetry_enabled(data_dir: &Path, explicit: Option<bool>) -> bool {
    if let Some(flag) = explicit {
        return flag;
    }
    if let Ok(env_val) = std::env::var("P2_TELEMETRY") {
        let trimmed = env_val.trim();
        if trimmed == "0" || trimmed.eq_ignore_ascii_case("false") {
            return false;
        }
        if trimmed == "1" || trimmed.eq_ignore_ascii_case("true") {
            return true;
        }
    }
    if let Ok(raw) = std::fs::read_to_string(data_dir.join("settings.json"))
        && let Ok(val) = serde_json::from_str::<Value>(raw.trim_start_matches('\u{feff}'))
        && let Some(flag) = val.get("telemetry").and_then(Value::as_bool)
    {
        return flag;
    }
    true
}

/// Adapter exposing `SqliteTelemetry` (`<data>/telemetry.sqlite`) as a `TelemetryBackend`.
pub struct SqliteTelemetryBackend(pub SqliteTelemetry);

impl TelemetryBackend for SqliteTelemetryBackend {
    fn find_traces(&self, query: TraceQuery) -> SyncFuture<Value> {
        let store = self.0.clone();
        Box::pin(async move {
            let page = store.find_traces_fallible(query).await?;
            let summaries: Vec<TraceSummary> = page.traces.iter().map(summarize_trace).collect();
            let mut obj = Map::new();
            obj.insert("traces".into(), serde_json::to_value(summaries)?);
            if let Some(cursor) = page.cursor {
                obj.insert("cursor".into(), Value::String(cursor));
            }
            Ok(Value::Object(obj))
        })
    }

    fn get_trace(&self, trace_id: String) -> SyncFuture<Value> {
        let store = self.0.clone();
        Box::pin(async move {
            let Some(trace) = store.get_trace_fallible(&trace_id).await? else {
                return Ok(Value::Null);
            };
            let mut spans = trace.spans;
            spans.sort_by(|a, b| a.started_ms.total_cmp(&b.started_ms));
            let truncated = spans.len() > MAX_SPANS;
            spans.truncate(MAX_SPANS);
            Ok(json!({
                "traceId": trace_id,
                "spans": serde_json::to_value(&spans)?,
                "truncated": truncated,
            }))
        })
    }

    fn find_metrics(&self, query: MetricQuery) -> SyncFuture<Value> {
        let store = self.0.clone();
        Box::pin(async move {
            let mut metrics = store.find_metrics_fallible(query).await?;
            let truncated = metrics.len() > MAX_METRICS;
            metrics.truncate(MAX_METRICS);
            Ok(json!({
                "metrics": serde_json::to_value(&metrics)?,
                "truncated": truncated,
            }))
        })
    }
}

fn ingest_contract(name: &str, variant: &str, batch_schema: Value) -> Result<Contract> {
    let host = HostKind::new("server", read_params(&[], "server")?)?;
    let members = IndexMap::from([(
        "ingest".into(),
        Member::Method {
            args: JsonValue::from_serde(batch_schema),
            result: JsonValue::from_serde(json!({
                "type": "object",
                "properties": {
                    "written": { "type": "number" },
                    "dropped": { "type": "number" },
                },
                "required": ["written", "dropped"],
            })),
        },
    )]);
    Contract::remote(
        name,
        1,
        Vec::new(),
        IndexMap::from([(variant.into(), members)]),
        Arc::new(move |_| host.address(JsonValue::Object(IndexMap::new()))),
    )
}

/// Server-hosted remote contract used by workers to upload telemetry batches.
pub fn worker_ingest_contract() -> Result<Contract> {
    ingest_contract("chord.telemetry.worker", "worker", worker_batch_schema())
}

/// Builds the Chord facet serving `chord.telemetry.page` and `chord.telemetry.worker`
/// and persisting ingested spans/metrics into `sqlite`.
pub fn ingest_facet(sqlite: SqliteTelemetry) -> Result<Facet> {
    struct Entry {
        contract: Arc<Contract>,
        variant: &'static str,
        ingest: IngestFacet,
    }
    struct Collector(Vec<Entry>);
    impl IngestRegistry for Collector {
        fn register(
            &mut self,
            name: &str,
            variant: &str,
            batch_schema: Value,
            facet: IngestFacet,
        ) -> Result<()> {
            let variant_static = if variant == "worker" {
                "worker"
            } else {
                "main"
            };
            let contract = Arc::new(ingest_contract(name, variant_static, batch_schema)?);
            self.0.push(Entry {
                contract,
                variant: variant_static,
                ingest: facet,
            });
            Ok(())
        }
    }

    let target_store = sqlite;
    let ingest = IngestFacet::new(IngestOptions {
        target: Arc::new(move |batch| target_store.ingest(batch)),
        admit: Arc::new(|principal| {
            matches!(
                serde_json::from_value::<Principal>(principal.clone()),
                Ok(Principal::Owner { .. } | Principal::Guest { .. })
            )
        }),
        worker_host: Arc::new(|principal| {
            if let Ok(Principal::Worker { session, .. }) =
                serde_json::from_value::<Principal>(principal.clone())
            {
                Some(format!("worker:{session}"))
            } else {
                None
            }
        }),
        principal_attributes: Arc::new(|principal| {
            let mut attrs = SpanAttributes::new();
            if let Ok(parsed) = serde_json::from_value::<Principal>(principal.clone()) {
                let tag = match parsed {
                    Principal::Owner { .. } => "owner".to_owned(),
                    Principal::Guest { id, .. } => format!("guest:{id}"),
                    Principal::Worker { session, .. } => format!("worker:{session}"),
                    _ => "unknown".to_owned(),
                };
                attrs.insert("chord.principal".into(), AttributeValue::String(tag));
            }
            attrs
        }),
    })?;

    let mut collector = Collector(Vec::new());
    ingest.register(&mut collector)?;
    let entries = Arc::new(collector.0);

    Ok(define_facet(move |host| {
        for entry in entries.iter() {
            let contract = entry.contract.clone();
            let variant = entry.variant;
            let facet = entry.ingest.clone();
            host.serve(
                contract,
                move |_, principal, object| {
                    let principal_val = principal.cloned().unwrap_or(Value::Null);
                    let mut members = Members::variant(variant);
                    if variant == "main" {
                        let page = Arc::new(facet.page(principal_val)?);
                        let incarnation = object.host().incarnation().to_owned();
                        let recorder = target_recorder_clock();
                        members.method("ingest", move |args, _| {
                            let page = page.clone();
                            let incarnation = incarnation.clone();
                            let recorder = recorder;
                            Box::pin(async move {
                                let receipt =
                                    page.ingest(&incarnation, recorder(), args.to_serde()?)?;
                                Ok(JsonValue::from_serde(serde_json::to_value(receipt)?))
                            })
                        })?;
                    } else {
                        let worker = Arc::new(facet.worker(&principal_val).ok_or_else(|| {
                            ChordError::new("forbidden", "chord.telemetry.worker: workers only")
                        })?);
                        members.method("ingest", move |args, _| {
                            let worker = worker.clone();
                            Box::pin(async move {
                                let receipt = worker.ingest(args.to_serde()?)?;
                                Ok(JsonValue::from_serde(serde_json::to_value(receipt)?))
                            })
                        })?;
                    }
                    Ok(members)
                },
                ServeOptions::default(),
            )?;
        }
        Ok(())
    }))
}

fn target_recorder_clock() -> fn() -> f64 {
    || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
    }
}

/// Dynamic query trait so server hosts can plug in SQLite or in-memory telemetry stores.
pub trait TelemetryBackend: Send + Sync + 'static {
    fn find_traces(&self, query: TraceQuery) -> SyncFuture<Value>;
    fn get_trace(&self, trace_id: String) -> SyncFuture<Value>;
    fn find_metrics(&self, query: MetricQuery) -> SyncFuture<Value>;
}

/// Adapter exposing `InMemoryTelemetryContext` as a `TelemetryBackend`.
pub struct InMemoryTelemetryBackend(pub InMemoryTelemetryContext);

impl TelemetryBackend for InMemoryTelemetryBackend {
    fn find_traces(&self, query: TraceQuery) -> SyncFuture<Value> {
        let store = self.0.clone();
        Box::pin(async move {
            let page = store
                .find_traces(query)
                .await
                .map_err(|e| Error::Invalid(e.to_string()))?;
            let summaries: Vec<TraceSummary> = page.traces.iter().map(summarize_trace).collect();
            let mut obj = Map::new();
            obj.insert("traces".into(), serde_json::to_value(summaries)?);
            if let Some(cursor) = page.cursor {
                obj.insert("cursor".into(), Value::String(cursor));
            }
            Ok(Value::Object(obj))
        })
    }

    fn get_trace(&self, trace_id: String) -> SyncFuture<Value> {
        let store = self.0.clone();
        Box::pin(async move {
            let Some(trace) = store.get_trace(&trace_id).await else {
                return Ok(Value::Null);
            };
            let mut spans = trace.spans;
            spans.sort_by(|a, b| a.started_ms.total_cmp(&b.started_ms));
            let truncated = spans.len() > MAX_SPANS;
            spans.truncate(MAX_SPANS);
            Ok(json!({
                "traceId": trace_id,
                "spans": serde_json::to_value(&spans)?,
                "truncated": truncated,
            }))
        })
    }

    fn find_metrics(&self, query: MetricQuery) -> SyncFuture<Value> {
        let store = self.0.clone();
        Box::pin(async move {
            let mut metrics = store.find_metrics(query).await;
            let truncated = metrics.len() > MAX_METRICS;
            metrics.truncate(MAX_METRICS);
            Ok(json!({
                "metrics": serde_json::to_value(&metrics)?,
                "truncated": truncated,
            }))
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FindTracesArgs {
    name: Option<String>,
    attributes: Option<SpanAttributes>,
    errors: Option<bool>,
    start_min: Option<f64>,
    start_max: Option<f64>,
    limit: Option<usize>,
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetTraceArgs {
    trace_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FindMetricsArgs {
    name: String,
    start_min: Option<f64>,
    start_max: Option<f64>,
    attributes: Option<SpanAttributes>,
}

fn parse_trace_query(args: FindTracesArgs) -> Result<TraceQuery> {
    let attributes = args.attributes.unwrap_or_default();
    if attributes.len() > MAX_FILTERS {
        return Err(Error::Invalid(format!("At most {MAX_FILTERS} attributes")));
    }
    Ok(TraceQuery {
        name: args.name,
        attributes,
        errors: args.errors.unwrap_or(false),
        start_min: args.start_min,
        start_max: args.start_max,
        limit: Some(args.limit.unwrap_or(DEFAULT_TRACES)),
        cursor: args.cursor,
    })
}

fn format_ms(value: f64) -> String {
    if value < 10.0 {
        format!("{value:.2} ms")
    } else {
        format!("{value:.0} ms")
    }
}

type TelemetryRun = Arc<dyn Fn(Value, Context) -> SyncFuture<(Value, String)> + Send + Sync>;

/// Builds the worker-side `p2-telemetry` extension (`telemetry_find_traces`, `telemetry_get_trace`, `telemetry_find_metrics`).
pub fn create_telemetry_extension(
    backend: Arc<dyn TelemetryBackend>,
) -> (Extension, IndexMap<String, ToolAccess>) {
    let make_tool =
        |name: &str, description: &str, parameters: Value, run: TelemetryRun| ToolRegistration {
            declaration: Tool {
                name: name.into(),
                description: description.into(),
                parameters,
                constrained_sampling: None,
            },
            sequential: false,
            prepare_arguments: None,
            cancel: None,
            structured_output_schema: Some(json!({})),
            output_limits: None,
            replay_safe: true,
            callers: Some(vec!["tools".into()]),
            deferred: true,
            execute: Arc::new(move |args, _, ctx| {
                let run = run.clone();
                Box::pin(async move {
                    match run(args, ctx.clone()).await {
                        Ok((value, text)) => Ok(ToolResult {
                            output: Some(vec![InputContent::Text(text.into())]),
                            structured_output: Some(value),
                            details: Some(json!({})),
                            ..Default::default()
                        }
                        .into()),
                        Err(err) => Ok(ToolResult {
                            output: Some(vec![InputContent::Text(err.to_string().into())]),
                            is_error: Some(true),
                            ..Default::default()
                        }
                        .into()),
                    }
                })
            }),
        };

    let b1 = backend.clone();
    let find_traces = make_tool(
        "telemetry_find_traces",
        "Traces in the owner's telemetry store, newest first.",
        json!({ "type": "object", "additionalProperties": true }),
        Arc::new(move |args, _| {
            let b = b1.clone();
            Box::pin(async move {
                let parsed: FindTracesArgs = serde_json::from_value(args)?;
                let query = parse_trace_query(parsed)?;
                let found = b.find_traces(query).await?;
                let lines: Vec<String> = found["traces"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|t| {
                        let id = t["traceId"].as_str().unwrap_or("");
                        let dur = format_ms(t["durationMs"].as_f64().unwrap_or(0.0));
                        let name = t["name"].as_str().unwrap_or("");
                        let spans = t["spans"].as_u64().unwrap_or(0);
                        let errors = t["errors"].as_u64().unwrap_or(0);
                        let err_part = if errors > 0 {
                            format!(", {errors} failed")
                        } else {
                            String::new()
                        };
                        format!("{id}  {dur}  {name}  {spans} spans{err_part}")
                    })
                    .collect();
                let mut text = if lines.is_empty() {
                    "No traces.".into()
                } else {
                    lines.join("\n")
                };
                if found.get("cursor").and_then(Value::as_str).is_some() {
                    text.push_str("\nMore: pass cursor for the next page.");
                }
                Ok((found, text))
            })
        }),
    );

    let b2 = backend.clone();
    let get_trace = make_tool(
        "telemetry_get_trace",
        "One trace as a span tree and structured spans.",
        json!({
            "type": "object",
            "properties": { "traceId": { "type": "string", "minLength": 1, "maxLength": 256 } },
            "required": ["traceId"]
        }),
        Arc::new(move |args, _| {
            let b = b2.clone();
            Box::pin(async move {
                let parsed: GetTraceArgs = serde_json::from_value(args)?;
                let trace = b.get_trace(parsed.trace_id.clone()).await?;
                if trace.is_null() {
                    let text = format!(
                        "No trace {} (never recorded, or older than the retention).",
                        parsed.trace_id
                    );
                    return Ok((json!({ "found": false, "text": text }), text));
                }
                let spans: Vec<StoredSpan> =
                    serde_json::from_value(trace["spans"].clone()).unwrap_or_default();
                let mut text = render_trace(&spans).map_err(|e| Error::Invalid(e.to_string()))?;
                if trace["truncated"].as_bool() == Some(true) {
                    text.push_str("\n(Only the first 2000 spans.)");
                }
                Ok((json!({ "found": true, "text": text, "trace": trace }), text))
            })
        }),
    );

    let b3 = backend;
    let find_metrics = make_tool(
        "telemetry_find_metrics",
        "A metric's 10 s aggregates.",
        json!({
            "type": "object",
            "properties": { "name": { "type": "string", "minLength": 1 } },
            "required": ["name"],
            "additionalProperties": true
        }),
        Arc::new(move |args, _| {
            let b = b3.clone();
            Box::pin(async move {
                let parsed: FindMetricsArgs = serde_json::from_value(args)?;
                let query = MetricQuery {
                    name: parsed.name,
                    start_min: parsed.start_min,
                    start_max: parsed.start_max,
                    attributes: parsed.attributes.unwrap_or_default(),
                };
                let found = b.find_metrics(query).await?;
                let count = found["metrics"].as_array().map_or(0, Vec::len);
                let mut text = if count == 0 {
                    "No rows.".into()
                } else {
                    format!("{count} metric rows.")
                };
                if found["truncated"].as_bool() == Some(true) {
                    text.push_str("\n(Only the first 1000 rows.)");
                }
                Ok((found, text))
            })
        }),
    );

    let mut access = IndexMap::new();
    for name in [
        "telemetry_find_traces",
        "telemetry_get_trace",
        "telemetry_find_metrics",
    ] {
        access.insert(
            name.into(),
            ToolAccess {
                main_only: false,
                locked: LockedTool::Never,
            },
        );
    }

    (
        define_extension(Extension {
            name: "p2-telemetry".into(),
            tools: vec![find_traces, get_trace, find_metrics],
            ..Default::default()
        }),
        access,
    )
}

pub fn register(registry: &mut PluginRegistry) -> Result<()> {
    let traces_contract = Arc::new(lookup("p2.traces")?);
    let shares_contract = Arc::new(lookup("p2.shares")?);
    let locks_contract = Arc::new(lookup("p2.session-locks")?);

    registry.register(
        "telemetry",
        FacetKind::Server,
        define_facet(move |host| {
            let shares_ref = shares_contract.clone();
            let locks_ref = locks_contract.clone();
            host.serve(
                traces_contract.clone(),
                move |_, principal, object| {
                    let parsed = principal
                        .and_then(|v| Principal::deserialize(v).ok())
                        .ok_or_else(|| ChordError::new("forbidden", "Telemetry is the owner's."))?;

                    let (variant, check_session): (&'static str, Option<String>) = match parsed {
                        Principal::Owner { .. } => ("owner", None),
                        Principal::Worker { session, .. } => ("agent", Some(session)),
                        _ => {
                            return Err(
                                ChordError::new("forbidden", "Telemetry is the owner's.").into()
                            );
                        }
                    };

                    let shares = check_session
                        .as_ref()
                        .and_then(|_| {
                            object.host().served_contract("p2.shares").map(|_| {
                                object.get(shares_ref.clone(), JsonValue::from_serde(json!({})))
                            })
                        })
                        .transpose()?;
                    let locks = check_session
                        .as_ref()
                        .and_then(|_| {
                            object.host().served_contract("p2.session-locks").map(|_| {
                                object.get(locks_ref.clone(), JsonValue::from_serde(json!({})))
                            })
                        })
                        .transpose()?;

                    let host_handle = object.host();
                    let check_access = Arc::new(move || -> Result<Arc<dyn TelemetryBackend>> {
                        if let Some(ref session) = check_session {
                            if let Some(ref locks_held) = locks
                                && let Ok(val) = locks_held
                                    .member("locks")
                                    .and_then(|m| m.value())
                                    .and_then(|v| v.to_json().to_serde())
                                && val.get(session).is_some_and(|v| !v.is_null())
                            {
                                return Err(ChordError::new(
                                    "forbidden",
                                    "Telemetry is the owner's: this session is locked.",
                                )
                                .into());
                            }
                            if let Some(ref shares_held) = shares {
                                let linked = shares_held
                                    .member("links")
                                    .and_then(|m| m.value())
                                    .and_then(|v| v.to_json().to_serde())
                                    .ok()
                                    .and_then(|v| v.as_array().cloned())
                                    .is_some_and(|arr| {
                                        arr.iter().any(|l| l["session"].as_str() == Some(session))
                                    });
                                let peopled = shares_held
                                    .member("people")
                                    .and_then(|m| m.value())
                                    .and_then(|v| v.to_json().to_serde())
                                    .ok()
                                    .and_then(|v| v.as_array().cloned())
                                    .is_some_and(|arr| {
                                        arr.iter().any(|p| p["session"].as_str() == Some(session))
                                    });
                                if linked || peopled {
                                    return Err(ChordError::new(
                                        "forbidden",
                                        "Telemetry is the owner's: this session is shared.",
                                    )
                                    .into());
                                }
                            }
                        }
                        service_of::<Arc<dyn TelemetryBackend>>(
                            &host_handle,
                            TELEMETRY_STORE_SERVICE,
                        )
                        .map(|arc| (*arc).clone())
                        .map_err(|_| ChordError::new("unavailable", TELEMETRY_OFF).into())
                    });

                    let mut members = Members::variant(variant);

                    {
                        let check = check_access.clone();
                        members.method("findTraces", move |args, _| {
                            let check = check.clone();
                            Box::pin(async move {
                                let store = check()?;
                                let parsed: FindTracesArgs =
                                    serde_json::from_value(args.to_serde()?)?;
                                let query = parse_trace_query(parsed)?;
                                Ok(JsonValue::from_serde(store.find_traces(query).await?))
                            })
                        })?;
                    }

                    {
                        let check = check_access.clone();
                        members.method("getTrace", move |args, _| {
                            let check = check.clone();
                            Box::pin(async move {
                                let store = check()?;
                                let parsed: GetTraceArgs =
                                    serde_json::from_value(args.to_serde()?)?;
                                Ok(JsonValue::from_serde(
                                    store.get_trace(parsed.trace_id).await?,
                                ))
                            })
                        })?;
                    }

                    {
                        let check = check_access;
                        members.method("findMetrics", move |args, _| {
                            let check = check.clone();
                            Box::pin(async move {
                                let store = check()?;
                                let parsed: FindMetricsArgs =
                                    serde_json::from_value(args.to_serde()?)?;
                                let query = MetricQuery {
                                    name: parsed.name,
                                    start_min: parsed.start_min,
                                    start_max: parsed.start_max,
                                    attributes: parsed.attributes.unwrap_or_default(),
                                };
                                Ok(JsonValue::from_serde(store.find_metrics(query).await?))
                            })
                        })?;
                    }

                    Ok(members)
                },
                ServeOptions::default(),
            )
        }),
    )?;

    let tools_contract = Arc::new(Contract::local(
        "p2.telemetry-tools",
        1,
        Vec::new(),
        IndexMap::new(),
    )?);
    registry.register(
        "telemetry",
        FacetKind::Worker,
        define_facet(move |host| {
            host.serve(
                tools_contract.clone(),
                move |_, _, object| {
                    let host_handle = object.host();
                    if let Ok(worker) = worker_of(&host_handle)
                        && let Ok(backend) = service_of::<Arc<dyn TelemetryBackend>>(
                            &host_handle,
                            TELEMETRY_STORE_SERVICE,
                        )
                    {
                        let (extension, access) = create_telemetry_extension((*backend).clone());
                        let ext_name = extension.name.clone();
                        let registries = worker.registries.clone();
                        registries
                            .extensions
                            .install_with_access(extension, &access)?;
                        let _cleanup = object.open(
                            |_| Box::pin(async { Ok(()) }),
                            move |_, _| {
                                let registries = registries.clone();
                                let ext_name = ext_name.clone();
                                Box::pin(async move {
                                    let _ = registries.extensions.uninstall(&ext_name);
                                    Ok(())
                                })
                            },
                        );
                    }
                    Ok(Members::new())
                },
                ServeOptions::default(),
            )
        }),
    )
}
