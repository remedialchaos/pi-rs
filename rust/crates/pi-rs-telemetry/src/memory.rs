//! In-memory recording, propagation, clock anchoring, and settled queries.
use crate::*;
use serde::{Deserialize, Serialize};
use std::{
    any::Any,
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

pub trait Clock: Send + Sync {
    fn wall_ms(&self) -> f64;
    fn monotonic_ms(&self) -> f64;
}
struct SystemClock {
    origin: Instant,
}
impl Clock for SystemClock {
    fn wall_ms(&self) -> f64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(duration) => duration.as_millis() as f64,
            Err(error) => -(error.duration().as_millis() as f64),
        }
    }
    fn monotonic_ms(&self) -> f64 {
        self.origin.elapsed().as_secs_f64() * 1000.0
    }
}
fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
pub struct TelemetryClock {
    source: Arc<dyn Clock>,
    anchor: Mutex<f64>,
}
impl TelemetryClock {
    pub fn new(source: Arc<dyn Clock>) -> Self {
        let anchor = source.wall_ms() - source.monotonic_ms();
        Self {
            source,
            anchor: Mutex::new(anchor),
        }
    }
    pub fn now(&self) -> f64 {
        let clock = self.source.monotonic_ms();
        let wall = self.source.wall_ms();
        let mut anchor = lock(&self.anchor);
        if (*anchor + clock - wall).abs() > 10.0 {
            *anchor = wall - clock;
        }
        microseconds(*anchor + clock)
    }
    fn monotonic_ms(&self) -> f64 {
        self.source.monotonic_ms()
    }
    pub(crate) fn interval_ms(&self) -> f64 {
        (self.source.wall_ms() / 10000.0).floor() * 10000.0
    }
}
fn system_clock() -> Arc<TelemetryClock> {
    static CLOCK: OnceLock<Arc<TelemetryClock>> = OnceLock::new();
    CLOCK
        .get_or_init(|| {
            Arc::new(TelemetryClock::new(Arc::new(SystemClock {
                origin: Instant::now(),
            })))
        })
        .clone()
}
pub fn telemetry_now() -> f64 {
    system_clock().now()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordedTelemetryEvent {
    pub name: String,
    pub attributes: SpanAttributes,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordedTelemetryLink {
    pub context: TelemetryCarrier,
    pub attributes: SpanAttributes,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedTelemetrySpan {
    pub id: u64,
    pub parent_id: Option<f64>,
    pub context: TelemetryCarrier,
    pub parent_context: Option<TelemetryCarrier>,
    pub links: Vec<RecordedTelemetryLink>,
    pub name: String,
    pub attributes: SpanAttributes,
    pub events: Vec<RecordedTelemetryEvent>,
    pub status: SpanStatus,
    pub settled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_sequence: Option<u64>,
}
struct MutableSpan {
    recorded: RecordedTelemetrySpan,
    started_ms: f64,
    clock: f64,
    ended_ms: f64,
    events: Vec<StoredEvent>,
    explicit_status: bool,
}
/// External recording sink receiving settled spans and recorded metric
/// measurements from `Context` and `Metrics` without exposing in-memory internals.
pub trait TelemetryRecorderSink: Send + Sync {
    fn record_span(&self, span: StoredSpan);
    fn record_measurement(&self, interval_ms: f64, measurement: RecordedTelemetryMeasurement);
}

pub(crate) struct State {
    scope: String,
    spans: Vec<MutableSpan>,
    next_end: u64,
    pub(crate) measurements: Vec<(f64, RecordedTelemetryMeasurement)>,
    pub(crate) clock: Arc<TelemetryClock>,
    sink: Option<Arc<dyn TelemetryRecorderSink>>,
}
#[derive(Clone)]
pub struct Context {
    pub(crate) state: Option<Arc<Mutex<State>>>,
    carrier: Option<TelemetryCarrier>,
    span: Option<usize>,
    inert: bool,
}
pub type InMemoryTelemetryContext = Context;
impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("carrier", &self.carrier)
            .field("inert", &self.inert)
            .finish_non_exhaustive()
    }
}
impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}
impl Context {
    pub fn new() -> Self {
        Self::with_clock(system_clock())
    }
    pub fn with_clock(clock: Arc<TelemetryClock>) -> Self {
        Self::with_state(clock, None)
    }
    pub fn with_sink(sink: Arc<dyn TelemetryRecorderSink>) -> Self {
        Self::with_state(system_clock(), Some(sink))
    }
    pub fn with_clock_and_sink(
        clock: Arc<TelemetryClock>,
        sink: Arc<dyn TelemetryRecorderSink>,
    ) -> Self {
        Self::with_state(clock, Some(sink))
    }
    fn with_state(
        clock: Arc<TelemetryClock>,
        sink: Option<Arc<dyn TelemetryRecorderSink>>,
    ) -> Self {
        Self {
            state: Some(Arc::new(Mutex::new(State {
                scope: uuid::Uuid::new_v4().to_string(),
                spans: vec![],
                next_end: 1,
                measurements: vec![],
                clock,
                sink,
            }))),
            carrier: None,
            span: None,
            inert: false,
        }
    }
    pub const fn noop() -> Self {
        Self {
            state: None,
            carrier: None,
            span: None,
            inert: true,
        }
    }
    fn root(&self) -> Self {
        Self {
            state: self.state.clone(),
            carrier: None,
            span: None,
            inert: self.state.is_none(),
        }
    }
    pub fn get_spans(&self) -> Vec<RecordedTelemetrySpan> {
        self.state
            .as_ref()
            .map(|s| lock(s).spans.iter().map(|s| s.recorded.clone()).collect())
            .unwrap_or_default()
    }
    pub fn get_metrics(&self) -> Vec<RecordedTelemetryMeasurement> {
        self.state
            .as_ref()
            .map(|s| {
                lock(s)
                    .measurements
                    .iter()
                    .map(|(_, m)| m.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn inject(&self, context: &Context) -> Option<TelemetryCarrier> {
        self.state.as_ref()?;
        context.carrier.clone()
    }
    pub fn extract(&self, carrier: Option<&TelemetryCarrier>) -> Context {
        if self.state.is_none() {
            return Self::noop();
        }
        let Some(carrier) = carrier else {
            return self.root();
        };
        let mut identity = TelemetryCarrier::new();
        for key in ["pi-memory-scope", "pi-memory-span", "pi-memory-trace"] {
            let Some(value) = carrier.get(key) else {
                return self.root();
            };
            identity.insert(key.into(), value.clone());
        }
        Self {
            state: self.state.clone(),
            carrier: Some(identity),
            span: None,
            inert: false,
        }
    }
    pub(crate) fn finish(&self, error: Option<&dyn Any>) {
        let (Some(state), Some(index)) = (&self.state, self.span) else {
            return;
        };
        let emitted = {
            let mut state = lock(state);
            let sequence = state.next_end;
            let clock = state.clock.monotonic_ms();
            let span = &mut state.spans[index];
            if span.recorded.settled {
                return;
            }
            if let Some(error) = error.filter(|_| !span.explicit_status) {
                let detail = error.downcast_ref::<ErrorDetails>().cloned().or_else(|| {
                    error
                        .downcast_ref::<std::io::Error>()
                        .map(|e| ErrorDetails {
                            name: "Error".into(),
                            message: e.to_string(),
                        })
                });
                span.recorded.status = SpanStatus::Error { error: detail };
            }
            span.recorded.settled = true;
            span.recorded.end_sequence = Some(sequence);
            span.ended_ms = microseconds(span.started_ms + clock - span.clock);
            let stored = state
                .sink
                .clone()
                .map(|sink| (sink, to_stored_span(&state.spans[index])));
            state.next_end += 1;
            stored
        };
        if let Some((sink, stored)) = emitted {
            sink.record_span(stored);
        }
    }
    fn mutate(&self, callback: impl FnOnce(&mut MutableSpan, f64)) {
        let (Some(state), Some(index)) = (&self.state, self.span) else {
            return;
        };
        let mut state = lock(state);
        let clock = state.clock.monotonic_ms();
        let span = &mut state.spans[index];
        if !span.recorded.settled {
            callback(span, clock);
        }
    }
    fn stored_spans(&self) -> Vec<StoredSpan> {
        let Some(state) = &self.state else {
            return vec![];
        };
        let mut spans: Vec<_> = lock(state)
            .spans
            .iter()
            .filter(|s| s.recorded.settled)
            .map(to_stored_span)
            .collect();
        spans.sort_by(|a, b| {
            a.started_ms
                .total_cmp(&b.started_ms)
                .then_with(|| js_cmp(&a.id, &b.id))
        });
        spans
    }
}
fn to_stored_span(span: &MutableSpan) -> StoredSpan {
    let r = &span.recorded;
    StoredSpan {
        id: carrier_id(&r.context),
        trace_id: r.context["pi-memory-trace"].clone(),
        parent_id: r.parent_context.as_ref().map(carrier_id),
        name: r.name.clone(),
        started_ms: span.started_ms,
        ended_ms: span.ended_ms,
        attributes: r.attributes.clone(),
        events: span.events.clone(),
        status: r.status.clone(),
        links: r
            .links
            .iter()
            .map(|link| StoredLink {
                trace_id: link.context["pi-memory-trace"].clone(),
                span_id: carrier_id(&link.context),
                attributes: link.attributes.clone(),
            })
            .collect(),
    }
}
pub fn carrier_id(carrier: &TelemetryCarrier) -> String {
    format!(
        "{}:{}",
        carrier
            .get("pi-memory-scope")
            .map(String::as_str)
            .unwrap_or("undefined"),
        carrier
            .get("pi-memory-span")
            .map(String::as_str)
            .unwrap_or("undefined")
    )
}
impl TelemetryContext for Context {
    fn metrics(&self) -> Metrics {
        Metrics {
            state: self.state.clone(),
        }
    }
    fn propagation(&self) -> Propagation {
        Propagation { root: self.root() }
    }
    fn open_span(&self, options: SpanOptions) -> Context {
        let Some(shared) = &self.state else {
            return Self::noop();
        };
        let mut state = lock(shared);
        if self.inert
            || self
                .span
                .is_some_and(|index| state.spans[index].recorded.settled)
        {
            return Self {
                state: self.state.clone(),
                carrier: None,
                span: None,
                inert: true,
            };
        }
        let id = state.spans.len() as u64 + 1;
        let trace = self
            .carrier
            .as_ref()
            .map(|c| c["pi-memory-trace"].clone())
            .unwrap_or_else(|| format!("{}:{id}", state.scope));
        let carrier: TelemetryCarrier = [
            ("pi-memory-scope".into(), state.scope.clone()),
            ("pi-memory-span".into(), id.to_string()),
            ("pi-memory-trace".into(), trace),
        ]
        .into_iter()
        .collect();
        let parent_id = self
            .carrier
            .as_ref()
            .filter(|c| c["pi-memory-scope"] == state.scope)
            .map(|c| c["pi-memory-span"].trim().parse().unwrap_or(f64::NAN));
        let links = options.links.into_iter().filter_map(copy_link).collect();
        let span = MutableSpan {
            started_ms: state.clock.now(),
            clock: state.clock.monotonic_ms(),
            ended_ms: 0.0,
            events: vec![],
            explicit_status: false,
            recorded: RecordedTelemetrySpan {
                id,
                parent_id,
                context: carrier.clone(),
                parent_context: self.carrier.clone(),
                links,
                name: options.name,
                attributes: copy_attributes(options.attributes),
                events: vec![],
                status: SpanStatus::Ok,
                settled: false,
                end_sequence: None,
            },
        };
        let index = state.spans.len();
        state.spans.push(span);
        Self {
            state: self.state.clone(),
            carrier: Some(carrier),
            span: Some(index),
            inert: false,
        }
    }
}
fn copy_link(link: SpanLink) -> Option<RecordedTelemetryLink> {
    Some(RecordedTelemetryLink {
        context: link.context.carrier?,
        attributes: copy_attributes(link.attributes),
    })
}
impl TelemetrySpan for Context {
    fn add_link(&self, link: SpanLink) {
        if let Some(link) = copy_link(link) {
            self.mutate(|span, _| span.recorded.links.push(link));
        }
    }
    fn add_event(&self, name: impl Into<String>, attributes: SpanAttributes) {
        let name = name.into();
        let attributes = copy_attributes(attributes);
        self.mutate(|span, clock| {
            span.recorded.events.push(RecordedTelemetryEvent {
                name: name.clone(),
                attributes: attributes.clone(),
            });
            span.events.push(StoredEvent {
                name,
                attributes,
                time_ms: microseconds(clock - span.clock),
            });
        });
    }
    fn set_attributes(&self, attributes: SpanAttributes) {
        self.mutate(|span, _| span.recorded.attributes.extend(copy_attributes(attributes)));
    }
    fn set_status(&self, status: SpanStatus) {
        self.mutate(|span, _| {
            span.recorded.status = status;
            span.explicit_status = true;
        });
    }
}
#[derive(Clone, Debug)]
pub struct Propagation {
    pub(crate) root: Context,
}
impl Propagation {
    pub fn same_instance(&self, other: &Self) -> bool {
        match (&self.root.state, &other.root.state) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        }
    }
}
impl TelemetryPropagation for Propagation {
    fn inject(&self, context: &Context) -> Option<TelemetryCarrier> {
        self.root.inject(context)
    }
    fn extract(&self, carrier: Option<&TelemetryCarrier>) -> Context {
        self.root.extract(carrier)
    }
}
impl TelemetryStore for Context {
    async fn get_trace(&self, trace_id: &str) -> Option<StoredTrace> {
        let spans: Vec<_> = self
            .stored_spans()
            .into_iter()
            .filter(|s| s.trace_id == trace_id)
            .collect();
        (!spans.is_empty()).then(|| StoredTrace {
            trace_id: trace_id.into(),
            spans,
        })
    }
    async fn find_traces(&self, query: TraceQuery) -> Result<TracePage, TelemetryError> {
        let limit = query.limit.unwrap_or(20);
        if limit == 0 {
            return Err(TelemetryError::InvalidLimit);
        }
        let after: Option<(f64, String)> = query
            .cursor
            .as_ref()
            .map(|cursor| serde_json::from_str(cursor))
            .transpose()
            .map_err(TelemetryError::InvalidCursor)?;
        let mut grouped: IndexMap<String, StoredTrace> = IndexMap::new();
        for span in self.stored_spans() {
            grouped
                .entry(span.trace_id.clone())
                .or_insert_with(|| StoredTrace {
                    trace_id: span.trace_id.clone(),
                    spans: vec![],
                })
                .spans
                .push(span);
        }
        let mut traces: Vec<_> = grouped
            .into_values()
            .filter(|trace| {
                let first = &trace.spans[0]; // INFALLIBLE: groups are created only when inserting a span.
                query.name.as_ref().is_none_or(|name| *name == first.name)
                    && query.start_min.is_none_or(|min| first.started_ms >= min)
                    && query.start_max.is_none_or(|max| first.started_ms <= max)
                    && after.as_ref().is_none_or(|(time, id)| {
                        first.started_ms < *time
                            || (first.started_ms == *time && js_cmp(&trace.trace_id, id).is_gt())
                    })
                    && (!query.errors
                        || trace
                            .spans
                            .iter()
                            .any(|s| matches!(s.status, SpanStatus::Error { .. })))
                    && trace
                        .spans
                        .iter()
                        .any(|s| matches_attributes(&s.attributes, &query.attributes))
            })
            .collect();
        traces.sort_by(|a, b| {
            b.spans[0]
                .started_ms
                .total_cmp(&a.spans[0].started_ms)
                .then_with(|| js_cmp(&a.trace_id, &b.trace_id))
        });
        let more = traces.len() > limit;
        traces.truncate(limit);
        let cursor = if more {
            traces
                .last()
                .map(|last| serde_json::to_string(&(last.spans[0].started_ms, &last.trace_id)))
                .transpose()
                .map_err(TelemetryError::InvalidCursor)?
        } else {
            None
        };
        Ok(TracePage { traces, cursor })
    }
    async fn find_metrics(&self, query: MetricQuery) -> Vec<StoredMetric> {
        let Some(state) = &self.state else {
            return vec![];
        };
        lock(state)
            .measurements
            .iter()
            .filter(|(interval, measurement)| {
                measurement.name == query.name
                    && query.start_min.is_none_or(|min| *interval >= min)
                    && query.start_max.is_none_or(|max| *interval <= max)
                    && matches_attributes(&measurement.attributes, &query.attributes)
            })
            .map(|(interval, m)| StoredMetric {
                interval_ms: *interval,
                name: m.name.clone(),
                kind: m.kind,
                unit: m.unit.clone(),
                attributes: m.attributes.clone(),
                count: 1,
                sum: m.value,
                min: m.value,
                max: m.value,
            })
            .collect()
    }
}
impl FallibleTelemetryStore for Context {
    type Error = TelemetryError;
    async fn find_traces_fallible(&self, query: TraceQuery) -> Result<TracePage, Self::Error> {
        TelemetryStore::find_traces(self, query).await
    }
    async fn get_trace_fallible(&self, trace_id: &str) -> Result<Option<StoredTrace>, Self::Error> {
        Ok(TelemetryStore::get_trace(self, trace_id).await)
    }
    async fn find_metrics_fallible(
        &self,
        query: MetricQuery,
    ) -> Result<Vec<StoredMetric>, Self::Error> {
        Ok(TelemetryStore::find_metrics(self, query).await)
    }
}
pub(crate) fn record(state: &Mutex<State>, measurement: RecordedTelemetryMeasurement) {
    let emitted = {
        let mut state = lock(state);
        let interval = state.clock.interval_ms();
        let emitted = state
            .sink
            .clone()
            .map(|sink| (sink, interval, measurement.clone()));
        state.measurements.push((interval, measurement));
        emitted
    };
    if let Some((sink, interval, measurement)) = emitted {
        sink.record_measurement(interval, measurement);
    }
}
