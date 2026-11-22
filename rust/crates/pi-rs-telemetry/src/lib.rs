//! Backend-independent telemetry contracts and the in-memory reference adapter.
#![forbid(unsafe_code)]

mod js;
pub mod memory;
pub mod metrics;
pub mod noop;
pub mod render;
pub mod schema;
pub use schema::*;
pub mod testing;

use indexmap::IndexMap;
pub use memory::{
    Clock, Context, InMemoryTelemetryContext, Propagation, RecordedTelemetryEvent,
    RecordedTelemetryLink, RecordedTelemetrySpan, TelemetryClock, TelemetryRecorderSink,
    telemetry_now,
};
pub use metrics::*;
pub use noop::*;
pub use render::render_trace;
use serde::{Deserialize, Serialize};
use std::{
    any::Any,
    future::Future,
    pin::Pin,
    task::{Context as TaskContext, Poll},
};

pub type TelemetryCarrier = IndexMap<String, String>;
pub type SpanAttributes = IndexMap<String, AttributeValue>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AttributeValue {
    String(String),
    Number(f64),
    Boolean(bool),
    Strings(Vec<String>),
    Numbers(Vec<f64>),
    Booleans(Vec<bool>),
    Undefined,
}
impl From<&str> for AttributeValue {
    fn from(value: &str) -> Self {
        Self::String(value.into())
    }
}
impl From<String> for AttributeValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}
impl From<f64> for AttributeValue {
    fn from(value: f64) -> Self {
        Self::Number(value)
    }
}
impl From<bool> for AttributeValue {
    fn from(value: bool) -> Self {
        Self::Boolean(value)
    }
}

#[derive(Clone, Debug)]
pub struct SpanLink {
    pub context: Context,
    pub attributes: SpanAttributes,
}
#[derive(Clone, Debug, Default)]
pub struct SpanOptions {
    pub name: String,
    pub attributes: SpanAttributes,
    pub links: Vec<SpanLink>,
}
impl SpanOptions {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorDetails {
    pub name: String,
    pub message: String,
}
impl std::fmt::Display for ErrorDetails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.name, self.message)
    }
}
impl std::error::Error for ErrorDetails {}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum SpanStatus {
    #[default]
    Ok,
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<ErrorDetails>,
    },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredEvent {
    pub name: String,
    pub time_ms: f64,
    pub attributes: SpanAttributes,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredLink {
    pub trace_id: String,
    pub span_id: String,
    #[serde(default)]
    pub attributes: SpanAttributes,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredSpan {
    pub id: String,
    pub trace_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub name: String,
    pub started_ms: f64,
    pub ended_ms: f64,
    pub attributes: SpanAttributes,
    pub events: Vec<StoredEvent>,
    pub links: Vec<StoredLink>,
    pub status: SpanStatus,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredTrace {
    pub trace_id: String,
    pub spans: Vec<StoredSpan>,
}
#[derive(Clone, Debug, Default)]
pub struct TraceQuery {
    pub name: Option<String>,
    pub attributes: SpanAttributes,
    pub errors: bool,
    pub start_min: Option<f64>,
    pub start_max: Option<f64>,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}
#[derive(Clone, Debug, Default)]
pub struct MetricQuery {
    pub name: String,
    pub start_min: Option<f64>,
    pub start_max: Option<f64>,
    pub attributes: SpanAttributes,
}
#[derive(Clone, Debug, PartialEq)]
pub struct TracePage {
    pub traces: Vec<StoredTrace>,
    pub cursor: Option<String>,
}
#[derive(Debug)]
pub enum TelemetryError {
    InvalidCursor(serde_json::Error),
    InvalidLimit,
}
impl std::fmt::Display for TelemetryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCursor(e) => write!(f, "invalid trace cursor: {e}"),
            Self::InvalidLimit => f.write_str("trace limit must be positive"),
        }
    }
}
impl std::error::Error for TelemetryError {}

/// An admitted callback whose completion settles its span. Polling never borrows its parent.
pub struct SpanFuture<Fut> {
    span: Context,
    callback: Pin<Box<Fut>>,
}
impl<T, E: Any, Fut: Future<Output = Result<T, E>>> Future for SpanFuture<Fut> {
    type Output = Result<T, E>;
    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let result = std::task::ready!(this.callback.as_mut().poll(cx));
        this.span
            .finish(result.as_ref().err().map(|error| error as &dyn Any));
        Poll::Ready(result)
    }
}

pub trait TelemetryContext: Clone {
    fn metrics(&self) -> Metrics;
    fn propagation(&self) -> Propagation;
    /// Called before constructing the returned future, admitting the span synchronously.
    fn open_span(&self, options: SpanOptions) -> Context;
    fn start_span<T, E: Any, F, Fut>(&self, options: SpanOptions, callback: F) -> SpanFuture<Fut>
    where
        F: FnOnce(Context) -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let span = self.open_span(options);
        let callback = Box::pin(callback(span.clone()));
        SpanFuture { span, callback }
    }
}
pub trait TelemetrySpan: TelemetryContext {
    fn add_link(&self, link: SpanLink);
    fn add_event(&self, name: impl Into<String>, attributes: SpanAttributes);
    fn set_attributes(&self, attributes: SpanAttributes);
    fn set_status(&self, status: SpanStatus);
}
pub trait TelemetryPropagation {
    fn inject(&self, context: &Context) -> Option<TelemetryCarrier>;
    fn extract(&self, carrier: Option<&TelemetryCarrier>) -> Context;
}
pub trait TelemetryStore {
    fn find_traces(
        &self,
        query: TraceQuery,
    ) -> impl Future<Output = Result<TracePage, TelemetryError>>;
    fn get_trace(&self, trace_id: &str) -> impl Future<Output = Option<StoredTrace>>;
    fn find_metrics(&self, query: MetricQuery) -> impl Future<Output = Vec<StoredMetric>>;
}

/// Fallible storage query contract for external backends (such as SQLite) that
/// must propagate I/O or storage failures instead of swallowing them.
pub trait FallibleTelemetryStore {
    type Error: std::error::Error + Send + Sync + 'static;
    fn find_traces_fallible(
        &self,
        query: TraceQuery,
    ) -> impl Future<Output = Result<TracePage, Self::Error>>;
    fn get_trace_fallible(
        &self,
        trace_id: &str,
    ) -> impl Future<Output = Result<Option<StoredTrace>, Self::Error>>;
    fn find_metrics_fallible(
        &self,
        query: MetricQuery,
    ) -> impl Future<Output = Result<Vec<StoredMetric>, Self::Error>>;
}

/// Schema definitions are serializable data. Identity helpers perform no validation upstream.
pub type TelemetrySchemaDefinition = serde_json::Value;
pub fn define_telemetry_schema<T>(schema: T) -> T {
    schema
}

#[derive(Clone)]
pub struct TypedSpanStarter {
    context: Context,
}
pub fn create_typed_span_starter(
    context: Context,
    _schemas: &[TelemetrySchemaDefinition],
) -> TypedSpanStarter {
    TypedSpanStarter { context }
}
impl TypedSpanStarter {
    pub fn start<T, E: Any, F, Fut>(
        &self,
        name: impl Into<String>,
        attributes: SpanAttributes,
        callback: F,
    ) -> impl Future<Output = Result<T, E>>
    where
        F: FnOnce(Context, TypedSpanStarter) -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        self.context.start_span(
            SpanOptions {
                name: name.into(),
                attributes,
                links: vec![],
            },
            |span| {
                let child = Self {
                    context: span.clone(),
                };
                callback(span, child)
            },
        )
    }
}

pub(crate) fn copy_attributes(attributes: SpanAttributes) -> SpanAttributes {
    attributes
        .into_iter()
        .filter(|(_, value)| !matches!(value, AttributeValue::Undefined))
        .collect()
}
pub(crate) fn matches_attributes(actual: &SpanAttributes, query: &SpanAttributes) -> bool {
    query.iter().all(|(key, value)| match value {
        AttributeValue::String(_) | AttributeValue::Number(_) | AttributeValue::Boolean(_) => {
            actual.get(key) == Some(value)
        }
        _ => false,
    })
}
pub(crate) fn js_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}
pub(crate) fn microseconds(value: f64) -> f64 {
    let scaled = value * 1000.0;
    (if scaled.fract() == -0.5 {
        scaled.ceil()
    } else {
        scaled.round()
    }) / 1000.0
}
